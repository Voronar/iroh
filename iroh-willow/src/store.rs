//! Store for entries, secrets, and capabilities used in the Willow engine.
//!
//! The storage backend is defined in the [`Storage`] trait and its associated types.
//!
//! The only implementation is currently an in-memory store at [`memory`].

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use iroh_blobs::store::{bao_tree::io::fsm::AsyncSliceReader, Map, MapEntry};
use rand_core::CryptoRngCore;
use traits::EntryStorage;

use crate::{
    form::{AuthForm, EntryForm, EntryOrForm, SubspaceForm, TimestampForm},
    interest::{CapSelector, UserSelector},
    proto::{
        data_model::Entry,
        data_model::{AuthorisedEntry, PayloadDigest},
        keys::{NamespaceId, NamespaceKind, NamespaceSecretKey, UserId},
    },
    store::traits::SecretStorage,
    util::time::system_time_now,
};

use self::auth::{Auth, AuthError};
use self::traits::Storage;

pub(crate) use self::traits::EntryOrigin;

pub(crate) mod auth;
pub mod memory;
pub mod traits;

/// Storage for the Willow engine.
///
/// Wraps a `Storage` instance and adds the [`Auth`] struct that uses the secret and caps storage to provide
/// authentication when inserting entries.
#[derive(Debug, Clone)]
pub(crate) struct Store<S: Storage> {
    storage: S,
    auth: Auth<S>,
}

impl<S: Storage> Store<S> {
    pub fn new(storage: S) -> Self {
        Self {
            auth: Auth::new(storage.secrets().clone(), storage.caps().clone()),
            storage,
        }
    }

    pub fn entries(&self) -> &S::Entries {
        self.storage.entries()
    }

    pub fn secrets(&self) -> &S::Secrets {
        self.storage.secrets()
    }

    pub fn payloads(&self) -> &S::Payloads {
        self.storage.payloads()
    }

    pub async fn read_entry_payload(&self, entry: &Entry) -> Result<Option<Bytes>> {
        let blob_entry = self.payloads().get(&entry.payload_digest().0).await?;

        let res = if let Some(blob_entry) = blob_entry {
            let mut reader = blob_entry.data_reader().await?;
            let data = reader
                .read_at(0, entry.payload_length().try_into()?)
                .await?;

            Some(data)
        } else {
            None
        };

        Ok(res)
    }

    pub fn auth(&self) -> &Auth<S> {
        &self.auth
    }

    pub async fn insert_entry(
        &self,
        entry: EntryOrForm,
        auth: AuthForm,
    ) -> Result<(AuthorisedEntry, bool)> {
        let user_id = auth.user_id();
        let entry = match entry {
            EntryOrForm::Entry(entry) => Ok(entry),
            EntryOrForm::Form(form) => self.form_to_entry(form, user_id).await,
        }?;
        let capability = match auth {
            AuthForm::Exact(cap) => cap,
            AuthForm::Any(user_id) => {
                let selector = CapSelector::for_entry(&entry, UserSelector::Exact(user_id));
                self.auth()
                    .get_write_cap(&selector)?
                    .ok_or_else(|| anyhow!("no write capability available"))?
            }
        };
        let secret_key = self
            .secrets()
            .get_user(&user_id)
            .context("Missing user keypair")?;

        // TODO(frando): This should use `authorisation_token_unchecked` if we uphold the invariant
        // that `user_id` is a pubkey for `secret_key`. However, that is `unsafe` at the moment
        // (but should not be, IMO).
        // Not using the `_unchecked` variant has the cost of an additional signature verification,
        // so significant.
        let token = capability.authorisation_token(&entry, secret_key)?;
        let authorised_entry = AuthorisedEntry::new_unchecked(entry, token);
        let inserted = self
            .entries()
            .ingest_entry(&authorised_entry, EntryOrigin::Local)?;
        Ok((authorised_entry, inserted))
    }

    pub fn create_namespace(
        &self,
        rng: &mut impl CryptoRngCore,
        kind: NamespaceKind,
        owner: UserId,
    ) -> Result<NamespaceId, AuthError> {
        let namespace_secret = NamespaceSecretKey::generate(rng, kind);
        let namespace_id = namespace_secret.id();
        self.secrets().insert_namespace(namespace_secret)?;
        self.auth().create_full_caps(namespace_id, owner)?;
        Ok(namespace_id)
    }

    /// Convert the form into an [`Entry`] by filling the fields with data from the environment and
    /// the provided [`Store`].
    ///
    /// `user_id` must be set to the user who is authenticating the entry.
    async fn form_to_entry(
        &self,
        form: EntryForm,
        user_id: UserId, // auth: AuthForm,
    ) -> anyhow::Result<Entry> {
        let timestamp = match form.timestamp {
            TimestampForm::Now => system_time_now(),
            TimestampForm::Exact(timestamp) => timestamp,
        };
        let subspace_id = match form.subspace_id {
            SubspaceForm::User => user_id,
            SubspaceForm::Exact(subspace) => subspace,
        };
        let (payload_digest, payload_length) = form.payload.submit(self.payloads()).await?;
        let entry = Entry::new(
            form.namespace_id,
            subspace_id,
            form.path,
            timestamp,
            payload_length,
            PayloadDigest(payload_digest),
        );
        Ok(entry)
    }

    pub fn remove_entries(&self, entries: Vec<Entry>) -> Result<Vec<bool>> {
        let mut res = vec![];

        for entry in entries {
            let is_removed = self.entries().remove_entry(&entry)?;

            res.push(is_removed);
        }

        Ok(res)
    }
}
