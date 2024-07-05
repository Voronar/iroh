use tokio::sync::broadcast;

use crate::{
    proto::{
        sync::{DataMessage, DataSendEntry, DataSendPayload},
        willow::AuthorisedEntry,
    },
    session::{
        channels::ChannelSenders, payload::DEFAULT_CHUNK_SIZE, static_tokens::StaticTokens, Error,
        SessionId,
    },
    store::{traits::Storage, Origin, Store},
};

use super::payload::{send_payload_chunked, CurrentPayload};

#[derive(derive_more::Debug)]
pub struct DataSender<S: Storage> {
    store: Store<S>,
    send: ChannelSenders,
    static_tokens: StaticTokens,
    session_id: SessionId,
}

impl<S: Storage> DataSender<S> {
    pub fn new(
        store: Store<S>,
        send: ChannelSenders,
        static_tokens: StaticTokens,
        session_id: SessionId,
    ) -> Self {
        Self {
            store,
            send,
            static_tokens,
            session_id,
        }
    }
    pub async fn run(mut self) -> Result<(), Error> {
        let mut stream = self.store.entries().subscribe(self.session_id);
        loop {
            match stream.recv().await {
                Ok(entry) => {
                    self.send_entry(entry).await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
                Err(broadcast::error::RecvError::Lagged(_count)) => {
                    // TODO: Queue another reconciliation
                }
            }
        }
        Ok(())
    }

    async fn send_entry(&mut self, authorised_entry: AuthorisedEntry) -> Result<(), Error> {
        let (entry, token) = authorised_entry.into_parts();
        let (static_token, dynamic_token) = token.into_parts();
        // TODO: partial payloads
        // let available = entry.payload_length;
        let static_token_handle = self
            .static_tokens
            .bind_and_send_ours(static_token, &self.send)
            .await?;
        let digest = entry.payload_digest;
        let msg = DataSendEntry {
            entry,
            static_token_handle,
            dynamic_token,
            offset: 0,
        };
        self.send.send(msg).await?;

        // TODO: only send payload if configured to do so and/or under size limit.
        let send_payloads = true;
        if send_payloads {
            send_payload_chunked(
                digest,
                self.store.payloads(),
                &self.send,
                DEFAULT_CHUNK_SIZE,
                |bytes| DataSendPayload { bytes }.into(),
            )
            .await?;
        }
        Ok(())
    }
}

#[derive(derive_more::Debug)]
pub struct DataReceiver<S: Storage> {
    store: Store<S>,
    current_payload: CurrentPayload,
    static_tokens: StaticTokens,
    session_id: SessionId,
}

impl<S: Storage> DataReceiver<S> {
    pub fn new(store: Store<S>, static_tokens: StaticTokens, session_id: SessionId) -> Self {
        Self {
            store,
            static_tokens,
            session_id,
            current_payload: Default::default(),
        }
    }

    pub async fn on_message(&mut self, message: DataMessage) -> Result<(), Error> {
        match message {
            DataMessage::SendEntry(message) => self.on_send_entry(message).await?,
            DataMessage::SendPayload(message) => self.on_send_payload(message).await?,
            DataMessage::SetMetadata(_) => {}
        }
        Ok(())
    }

    async fn on_send_entry(&mut self, message: DataSendEntry) -> Result<(), Error> {
        self.current_payload.assert_inactive()?;
        let authorised_entry = self
            .static_tokens
            .authorise_entry_eventually(
                message.entry,
                message.static_token_handle,
                message.dynamic_token,
            )
            .await?;
        self.store
            .entries()
            .ingest(&authorised_entry, Origin::Remote(self.session_id))?;
        self.current_payload
            .set(authorised_entry.into_entry(), None)?;
        Ok(())
    }

    async fn on_send_payload(&mut self, message: DataSendPayload) -> Result<(), Error> {
        self.current_payload
            .recv_chunk(self.store.payloads(), message.bytes)
            .await?;
        if self.current_payload.is_complete() {
            self.current_payload.finalize().await?;
        }
        Ok(())
    }
}
