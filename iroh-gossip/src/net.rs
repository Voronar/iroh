//! Networking for the `iroh-gossip` protocol

use anyhow::{anyhow, Context};
use bytes::{Bytes, BytesMut};
use futures_lite::{stream::Stream, StreamExt};
use futures_util::future::FutureExt;
use genawaiter::sync::{Co, Gen};
use iroh_net::{
    conn_manager::{ConnDirection, ConnInfo, ConnManager},
    endpoint::Connection,
    key::PublicKey,
    AddrInfo, Endpoint, NodeAddr,
};
use rand::rngs::StdRng;
use rand_core::SeedableRng;
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, task::Poll, time::Instant};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error_span, trace, warn, Instrument};

use self::util::{read_message, write_message, Timers};
use crate::proto::{self, PeerData, Scope, TopicId};

pub mod util;

/// ALPN protocol name
pub const GOSSIP_ALPN: &[u8] = b"/iroh-gossip/0";
/// Maximum message size is limited currently. The limit is more-or-less arbitrary.
// TODO: Make the limit configurable.
pub const MAX_MESSAGE_SIZE: usize = 4096;

/// Channel capacity for all subscription broadcast channels (single)
const SUBSCRIBE_ALL_CAP: usize = 2048;
/// Channel capacity for topic subscription broadcast channels (one per topic)
const SUBSCRIBE_TOPIC_CAP: usize = 2048;
/// Channel capacity for the send queue (one per connection)
const SEND_QUEUE_CAP: usize = 64;
/// Channel capacity for the ToActor message queue (single)
const TO_ACTOR_CAP: usize = 64;
/// Channel capacity for the InEvent message queue (single)
const IN_EVENT_CAP: usize = 1024;
/// Channel capacity for endpoint change message queue (single)
const ON_ENDPOINTS_CAP: usize = 64;

/// Events emitted from the gossip protocol
pub type Event = proto::Event<PublicKey>;
/// Commands for the gossip protocol
pub type Command = proto::Command<PublicKey>;

type InEvent = proto::InEvent<PublicKey>;
type OutEvent = proto::OutEvent<PublicKey>;
type Timer = proto::Timer<PublicKey>;
type ProtoMessage = proto::Message<PublicKey>;

/// Publish and subscribe on gossiping topics.
///
/// Each topic is a separate broadcast tree with separate memberships.
///
/// A topic has to be joined before you can publish or subscribe on the topic.
/// To join the swarm for a topic, you have to know the [`PublicKey`] of at least one peer that also joined the topic.
///
/// Messages published on the swarm will be delivered to all peers that joined the swarm for that
/// topic. You will also be relaying (gossiping) messages published by other peers.
///
/// With the default settings, the protocol will maintain up to 5 peer connections per topic.
///
/// Even though the [`Gossip`] is created from a [`Endpoint`], it does not accept connections
/// itself. You should run an accept loop on the [`Endpoint`] yourself, check the ALPN protocol of incoming
/// connections, and if the ALPN protocol equals [`GOSSIP_ALPN`], forward the connection to the
/// gossip actor through [Self::handle_connection].
///
/// The gossip actor will, however, initiate new connections to other peers by itself.
#[derive(Debug, Clone)]
pub struct Gossip {
    to_actor_tx: mpsc::Sender<ToActor>,
    on_endpoints_tx: mpsc::Sender<Vec<iroh_net::config::Endpoint>>,
    _actor_handle: Arc<JoinHandle<anyhow::Result<()>>>,
}

impl Gossip {
    /// Spawn a gossip actor and get a handle for it
    pub fn from_endpoint(endpoint: Endpoint, config: proto::Config, my_addr: &AddrInfo) -> Self {
        let peer_id = endpoint.node_id();
        let conn_manager = ConnManager::new(endpoint.clone(), GOSSIP_ALPN);
        let state = proto::State::new(
            peer_id,
            encode_peer_data(my_addr).unwrap(),
            config,
            rand::rngs::StdRng::from_entropy(),
        );
        let (to_actor_tx, to_actor_rx) = mpsc::channel(TO_ACTOR_CAP);
        let (in_event_tx, in_event_rx) = mpsc::channel(IN_EVENT_CAP);
        let (on_endpoints_tx, on_endpoints_rx) = mpsc::channel(ON_ENDPOINTS_CAP);

        let me = endpoint.node_id().fmt_short();
        let actor = Actor {
            endpoint,
            state,
            conn_manager,
            conn_tasks: Default::default(),
            to_actor_rx,
            in_event_rx,
            in_event_tx,
            on_endpoints_rx,
            conn_send_tx: Default::default(),
            pending_sends: Default::default(),
            timers: Timers::new(),
            subscribers_all: None,
            subscribers_topic: Default::default(),
        };

        let actor_handle = tokio::spawn(
            async move {
                if let Err(err) = actor.run().await {
                    warn!("gossip actor closed with error: {err:?}");
                    Err(err)
                } else {
                    Ok(())
                }
            }
            .instrument(error_span!("gossip", %me)),
        );
        Self {
            to_actor_tx,
            on_endpoints_tx,
            _actor_handle: Arc::new(actor_handle),
        }
    }

    /// Join a topic and connect to peers.
    ///
    ///
    /// This method only asks for [`PublicKey`]s. You must supply information on how to
    /// connect to these peers manually before, by calling [`Endpoint::add_node_addr`] on
    /// the underlying [`Endpoint`].
    ///
    /// This method returns a future that completes once the request reached the local actor.
    /// This completion returns a [`JoinTopicFut`] which completes once at least peer was joined
    /// successfully and the swarm thus becomes operational.
    ///
    /// The [`JoinTopicFut`] has no timeout, so it will remain pending indefinitely if no peer
    /// could be contacted. Usually you will want to add a timeout yourself.
    ///
    /// TODO: Resolve to an error once all connection attempts failed.
    pub async fn join(
        &self,
        topic: TopicId,
        peers: Vec<PublicKey>,
    ) -> anyhow::Result<JoinTopicFut> {
        let (tx, rx) = oneshot::channel();
        self.send(ToActor::Join(topic, peers, tx)).await?;
        Ok(JoinTopicFut(rx))
    }

    /// Quit a topic.
    ///
    /// This sends a disconnect message to all active peers and then drops the state
    /// for this topic.
    pub async fn quit(&self, topic: TopicId) -> anyhow::Result<()> {
        self.send(ToActor::Quit(topic)).await?;
        Ok(())
    }

    /// Broadcast a message on a topic to all peers in the swarm.
    ///
    /// This does not join the topic automatically, so you have to call [`Self::join`] yourself
    /// for messages to be broadcast to peers.
    ///
    /// Messages with the same content are only delivered once.
    pub async fn broadcast(&self, topic: TopicId, message: Bytes) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(ToActor::Broadcast(topic, message, Scope::Swarm, tx))
            .await?;
        rx.await??;
        Ok(())
    }

    /// Broadcast a message on a topic to the immediate neighbors.
    ///
    /// This does not join the topic automatically, so you have to call [`Self::join`] yourself
    /// for messages to be broadcast to peers.
    pub async fn broadcast_neighbors(&self, topic: TopicId, message: Bytes) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(ToActor::Broadcast(topic, message, Scope::Neighbors, tx))
            .await?;
        rx.await??;
        Ok(())
    }

    /// Subscribe to messages and event notifications for a topic.
    ///
    /// Does not join the topic automatically, so you have to call [`Self::join`] yourself
    /// to actually receive messages.
    pub async fn subscribe(&self, topic: TopicId) -> anyhow::Result<broadcast::Receiver<Event>> {
        let (tx, rx) = oneshot::channel();
        self.send(ToActor::Subscribe(topic, tx)).await?;
        let res = rx.await.map_err(|_| anyhow!("subscribe_tx dropped"))??;
        Ok(res)
    }

    /// Subscribe to all events published on topics that you joined.
    ///
    /// Note that this method takes self by value. Usually you would clone the [`Gossip`] handle.
    /// before.
    pub fn subscribe_all(
        self,
    ) -> impl Stream<Item = Result<(TopicId, Event), broadcast::error::RecvError>> {
        Gen::new(|co| async move {
            if let Err(err) = self.subscribe_all0(&co).await {
                warn!("subscribe_all produced error: {err:?}");
                co.yield_(Err(broadcast::error::RecvError::Closed)).await
            }
        })
    }

    async fn subscribe_all0(
        &self,
        co: &Co<Result<(TopicId, Event), broadcast::error::RecvError>>,
    ) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(ToActor::SubscribeAll(tx)).await?;
        let mut res = rx.await??;
        loop {
            let event = res.recv().await;
            co.yield_(event).await;
        }
    }

    /// Handle an incoming [`Connection`].
    ///
    /// Make sure to check the ALPN protocol yourself before passing the connection.
    pub async fn handle_connection(&self, conn: Connection) -> anyhow::Result<()> {
        self.send(ToActor::ConnIncoming(conn)).await?;
        Ok(())
    }

    /// Set info on our local endpoints.
    ///
    /// This will be sent to peers on Neighbor and Join requests so that they can connect directly
    /// to us.
    ///
    /// This is only best effort, and will drop new events if backed up.
    pub fn update_endpoints(&self, endpoints: &[iroh_net::config::Endpoint]) -> anyhow::Result<()> {
        let endpoints = endpoints.to_vec();
        self.on_endpoints_tx
            .try_send(endpoints)
            .map_err(|_| anyhow!("endpoints channel dropped"))?;
        Ok(())
    }

    async fn send(&self, event: ToActor) -> anyhow::Result<()> {
        self.to_actor_tx
            .send(event)
            .await
            .map_err(|_| anyhow!("gossip actor dropped"))
    }
}

/// Future that completes once at least one peer is joined for this topic.
///
/// The future has no timeout, so it will remain pending indefinitely if no peer
/// could be contacted. Usually you will want to add a timeout yourself.
///
/// TODO: Optionally resolve to an error once all connection attempts failed.
#[derive(Debug)]
pub struct JoinTopicFut(oneshot::Receiver<anyhow::Result<TopicId>>);
impl Future for JoinTopicFut {
    type Output = anyhow::Result<TopicId>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let res = Pin::new(&mut self.0).poll(cx);
        match res {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_err)) => Poll::Ready(Err(anyhow!("gossip actor dropped"))),
            Poll::Ready(Ok(res)) => Poll::Ready(res),
        }
    }
}

/// Input messages for the gossip [`Actor`].
#[derive(derive_more::Debug)]
enum ToActor {
    /// Handle a new incoming QUIC connection.
    ConnIncoming(iroh_net::endpoint::Connection),
    /// Join a topic with a list of peers. Reply with oneshot once at least one peer joined.
    Join(
        TopicId,
        Vec<PublicKey>,
        #[debug(skip)] oneshot::Sender<anyhow::Result<TopicId>>,
    ),
    /// Leave a topic, send disconnect messages and drop all state.
    Quit(TopicId),
    /// Broadcast a message on a topic.
    Broadcast(
        TopicId,
        #[debug("<{}b>", _1.len())] Bytes,
        Scope,
        #[debug(skip)] oneshot::Sender<anyhow::Result<()>>,
    ),
    /// Subscribe to a topic. Return oneshot which resolves to a broadcast receiver for events on a
    /// topic.
    Subscribe(
        TopicId,
        #[debug(skip)] oneshot::Sender<anyhow::Result<broadcast::Receiver<Event>>>,
    ),
    /// Subscribe to a topic. Return oneshot which resolves to a broadcast receiver for events on a
    /// topic.
    SubscribeAll(
        #[debug(skip)] oneshot::Sender<anyhow::Result<broadcast::Receiver<(TopicId, Event)>>>,
    ),
}

/// Actor that sends and handles messages between the connection and main state loops
struct Actor {
    /// Protocol state
    state: proto::State<PublicKey, StdRng>,
    endpoint: Endpoint,
    /// Connection manager to dial and accept connections.
    conn_manager: ConnManager,
    /// Input messages to the actor
    to_actor_rx: mpsc::Receiver<ToActor>,
    /// Sender for the state input (cloned into the connection loops)
    in_event_tx: mpsc::Sender<InEvent>,
    /// Input events to the state (emitted from the connection loops)
    in_event_rx: mpsc::Receiver<InEvent>,
    /// Updates of discovered endpoint addresses
    on_endpoints_rx: mpsc::Receiver<Vec<iroh_net::config::Endpoint>>,
    /// Queued timers
    timers: Timers<Timer>,
    /// Channels to send outbound messages into the connection loops
    conn_send_tx: HashMap<PublicKey, mpsc::Sender<ProtoMessage>>,
    /// Connection loop tasks
    conn_tasks: JoinSet<(PublicKey, anyhow::Result<()>)>,
    /// Queued messages that were to be sent before a dial completed
    pending_sends: HashMap<PublicKey, Vec<ProtoMessage>>,
    /// Broadcast senders for active topic subscriptions from the application
    subscribers_topic: HashMap<TopicId, broadcast::Sender<Event>>,
    /// Broadcast senders for wildcard subscriptions from the application
    subscribers_all: Option<broadcast::Sender<(TopicId, Event)>>,
}

impl Drop for Actor {
    fn drop(&mut self) {
        self.conn_tasks.abort_all();
    }
}

impl Actor {
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut i = 0;
        loop {
            i += 1;
            trace!(?i, "tick");
            tokio::select! {
                biased;
                msg = self.to_actor_rx.recv() => {
                    trace!(?i, "tick: to_actor_rx");
                    match msg {
                        Some(msg) => self.handle_to_actor_msg(msg, Instant::now()).await?,
                        None => {
                            debug!("all gossip handles dropped, stop gossip actor");
                            break;
                        }
                    }
                },
                new_endpoints = self.on_endpoints_rx.recv() => {
                    match new_endpoints {
                        Some(endpoints) => {
                            let addr = self.endpoint.my_addr_with_endpoints(endpoints)?;
                            let peer_data = encode_peer_data(&addr.info)?;
                            self.handle_in_event(InEvent::UpdatePeerData(peer_data), Instant::now()).await?;
                        }
                        None => {
                            debug!("endpoint change handle dropped, stopping gossip actor");
                            break;
                        }
                    }
                }
                Some(res) = self.conn_manager.next() => {
                    trace!(?i, "tick: conn_manager");
                    match res {
                        Ok(conn) => self.handle_new_connection(conn),
                        Err(err) => {
                            self.handle_in_event(InEvent::PeerDisconnected(err.node_id), Instant::now()).await?;
                        }
                    }
                }
                Some(res) = self.conn_tasks.join_next(), if !self.conn_tasks.is_empty() => {
                    match res {
                        Err(err) if !err.is_cancelled() => warn!(?err, "connection loop panicked"),
                        Err(_err) => {},
                        Ok((node_id, result)) => {
                            self.conn_manager.remove(&node_id);
                            self.conn_send_tx.remove(&node_id);
                            self.handle_in_event(InEvent::PeerDisconnected(node_id), Instant::now()).await?;
                            match result {
                                Ok(()) => debug!(peer=%node_id.fmt_short(), "connection closed without error"),
                                Err(err) => debug!(peer=%node_id.fmt_short(), "connection closed with error {err:?}"),
                            }
                        }
                    }
                }
                event = self.in_event_rx.recv() => {
                    trace!(?i, "tick: in_event_rx");
                    match event {
                        Some(event) => {
                            self.handle_in_event(event, Instant::now()).await.context("in_event_rx.recv -> handle_in_event")?;
                        }
                        None => unreachable!()
                    }
                }
                drain = self.timers.wait_and_drain() => {
                    trace!(?i, "tick: timers");
                    let now = Instant::now();
                    for (_instant, timer) in drain {
                        self.handle_in_event(InEvent::TimerExpired(timer), now).await.context("timers.drain_expired -> handle_in_event")?;
                    }
                }

            }
        }
        Ok(())
    }

    async fn handle_to_actor_msg(&mut self, msg: ToActor, now: Instant) -> anyhow::Result<()> {
        trace!("handle to_actor  {msg:?}");
        match msg {
            ToActor::ConnIncoming(conn) => {
                if let Err(err) = self.conn_manager.handle_connection(conn) {
                    warn!(?err, "failed to accept connection");
                }
            }
            ToActor::Join(topic_id, peers, reply) => {
                self.handle_in_event(InEvent::Command(topic_id, Command::Join(peers)), now)
                    .await?;
                if self.state.has_active_peers(&topic_id) {
                    // If the active_view contains at least one peer, reply now
                    reply.send(Ok(topic_id)).ok();
                } else {
                    // Otherwise, wait for any peer to come up as neighbor.
                    let sub = self.subscribe(topic_id);
                    tokio::spawn(async move {
                        let res = wait_for_neighbor_up(sub).await;
                        let res = res.map(|_| topic_id);
                        reply.send(res).ok();
                    });
                }
            }
            ToActor::Quit(topic_id) => {
                self.handle_in_event(InEvent::Command(topic_id, Command::Quit), now)
                    .await?;
                self.subscribers_topic.remove(&topic_id);
            }
            ToActor::Broadcast(topic_id, message, scope, reply) => {
                self.handle_in_event(
                    InEvent::Command(topic_id, Command::Broadcast(message, scope)),
                    now,
                )
                .await?;
                reply.send(Ok(())).ok();
            }
            ToActor::Subscribe(topic_id, reply) => {
                let rx = self.subscribe(topic_id);
                reply.send(Ok(rx)).ok();
            }
            ToActor::SubscribeAll(reply) => {
                let rx = self.subscribe_all();
                reply.send(Ok(rx)).ok();
            }
        };
        Ok(())
    }

    async fn handle_in_event(&mut self, event: InEvent, now: Instant) -> anyhow::Result<()> {
        if matches!(event, InEvent::TimerExpired(_)) {
            trace!("handle in_event  {event:?}");
        } else {
            debug!("handle in_event  {event:?}");
        };
        let out = self.state.handle(event, now);
        for event in out {
            if matches!(event, OutEvent::ScheduleTimer(_, _)) {
                trace!("handle out_event {event:?}");
            } else {
                debug!("handle out_event {event:?}");
            };
            match event {
                OutEvent::SendMessage(peer_id, message) => {
                    if let Some(send) = self.conn_send_tx.get(&peer_id) {
                        if let Err(_err) = send.send(message).await {
                            warn!("conn receiver for {peer_id:?} dropped");
                            self.conn_send_tx.remove(&peer_id);
                            self.conn_manager.remove(&peer_id);
                        }
                    } else {
                        if !self.conn_manager.is_pending(&peer_id) {
                            debug!(peer = ?peer_id, "dial");
                            self.conn_manager.dial(peer_id);
                        }
                        // TODO: Enforce max length
                        self.pending_sends.entry(peer_id).or_default().push(message);
                    }
                }
                OutEvent::EmitEvent(topic_id, event) => {
                    if let Some(sender) = self.subscribers_all.as_mut() {
                        if let Err(_event) = sender.send((topic_id, event.clone())) {
                            self.subscribers_all = None;
                        }
                    }
                    if let Some(sender) = self.subscribers_topic.get(&topic_id) {
                        // Only error case is that all [broadcast::Receivers] have been dropped.
                        // If so, remove the sender as well.
                        if let Err(_event) = sender.send(event) {
                            self.subscribers_topic.remove(&topic_id);
                        }
                    }
                }
                OutEvent::ScheduleTimer(delay, timer) => {
                    self.timers.insert(now + delay, timer);
                }
                OutEvent::DisconnectPeer(peer) => {
                    self.conn_send_tx.remove(&peer);
                    self.pending_sends.remove(&peer);
                    if let Some(conn) = self.conn_manager.remove(&peer) {
                        conn.close(0u8.into(), b"close from disconnect");
                    }
                }
                OutEvent::PeerData(node_id, data) => match decode_peer_data(&data) {
                    Err(err) => warn!("Failed to decode {data:?} from {node_id}: {err}"),
                    Ok(info) => {
                        debug!(peer = ?node_id, "add known addrs: {info:?}");
                        let node_addr = NodeAddr { node_id, info };
                        if let Err(err) = self.endpoint.add_node_addr(node_addr) {
                            debug!(peer = ?node_id, "add known failed: {err:?}");
                        }
                    }
                },
            }
        }
        Ok(())
    }

    fn handle_new_connection(&mut self, new_conn: ConnInfo) {
        let ConnInfo {
            conn,
            node_id,
            direction,
        } = new_conn;
        let (send_tx, send_rx) = mpsc::channel(SEND_QUEUE_CAP);
        self.conn_send_tx.insert(node_id, send_tx.clone());

        // Spawn a task for this connection
        let pending_sends = self.pending_sends.remove(&node_id);
        let in_event_tx = self.in_event_tx.clone();
        debug!(peer=%node_id.fmt_short(), ?direction, "connection established");
        self.conn_tasks.spawn(
            connection_loop(
                node_id,
                conn,
                direction,
                send_rx,
                in_event_tx,
                pending_sends,
            )
            .map(move |r| (node_id, r))
            .instrument(error_span!("gossip_conn", peer = %node_id.fmt_short())),
        );
    }

    fn subscribe_all(&mut self) -> broadcast::Receiver<(TopicId, Event)> {
        if let Some(tx) = self.subscribers_all.as_mut() {
            tx.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(SUBSCRIBE_ALL_CAP);
            self.subscribers_all = Some(tx);
            rx
        }
    }

    fn subscribe(&mut self, topic_id: TopicId) -> broadcast::Receiver<Event> {
        if let Some(tx) = self.subscribers_topic.get(&topic_id) {
            tx.subscribe()
        } else {
            let (tx, rx) = broadcast::channel(SUBSCRIBE_TOPIC_CAP);
            self.subscribers_topic.insert(topic_id, tx);
            rx
        }
    }
}

async fn wait_for_neighbor_up(mut sub: broadcast::Receiver<Event>) -> anyhow::Result<()> {
    loop {
        match sub.recv().await {
            Ok(Event::NeighborUp(_neighbor)) => break Ok(()),
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                break Err(anyhow!("Failed to join swarm: channel closed"))
            }
        }
    }
}

async fn connection_loop(
    from: PublicKey,
    conn: Connection,
    direction: ConnDirection,
    mut send_rx: mpsc::Receiver<ProtoMessage>,
    in_event_tx: mpsc::Sender<InEvent>,
    mut pending_sends: Option<Vec<ProtoMessage>>,
) -> anyhow::Result<()> {
    let (mut send, mut recv) = match direction {
        ConnDirection::Accept => conn.accept_bi().await?,
        ConnDirection::Dial => conn.open_bi().await?,
    };
    let mut send_buf = BytesMut::new();
    let mut recv_buf = BytesMut::new();

    // Forward queued pending sends
    if let Some(mut send_queue) = pending_sends.take() {
        for msg in send_queue.drain(..) {
            write_message(&mut send, &mut send_buf, &msg).await?;
        }
    }

    // loop over sending and receiving messages
    loop {
        tokio::select! {
            biased;
            // If `send_rx` is closed,
            // stop selecting it but don't quit.
            // We are not going to use connection for sending anymore,
            // but the other side may still want to use it to
            // send data to us.
            Some(msg) = send_rx.recv(), if !send_rx.is_closed() => {
                write_message(&mut send, &mut send_buf, &msg).await?
            }

            msg = read_message(&mut recv, &mut recv_buf) => {
                let msg = msg?;
                match msg {
                    None => break,
                    Some(msg) => in_event_tx.send(InEvent::RecvMessage(from, msg)).await?
                }
            }
        }
    }
    Ok(())
}

fn encode_peer_data(info: &AddrInfo) -> anyhow::Result<PeerData> {
    let bytes = postcard::to_stdvec(info)?;
    anyhow::ensure!(!bytes.is_empty(), "encoding empty peer data: {:?}", info);
    Ok(PeerData::new(bytes))
}

fn decode_peer_data(peer_data: &PeerData) -> anyhow::Result<AddrInfo> {
    let bytes = peer_data.as_bytes();
    if bytes.is_empty() {
        return Ok(AddrInfo::default());
    }
    let info = postcard::from_bytes(bytes)?;
    Ok(info)
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use iroh_net::key::SecretKey;
    use iroh_net::relay::{RelayMap, RelayMode};
    use tokio::spawn;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use tracing::info;

    use super::*;

    async fn create_endpoint(
        rng: &mut rand_chacha::ChaCha12Rng,
        relay_map: RelayMap,
    ) -> anyhow::Result<Endpoint> {
        Endpoint::builder()
            .secret_key(SecretKey::generate_with_rng(rng))
            .alpns(vec![GOSSIP_ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(relay_map))
            .insecure_skip_relay_cert_verify(true)
            .bind(0)
            .await
    }

    async fn endpoint_loop(
        endpoint: Endpoint,
        gossip: Gossip,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                conn = endpoint.accept() => match conn {
                    None => break,
                    Some(conn) => gossip.handle_connection(conn.await?).await?
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn gossip_net_smoke() {
        let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(1);
        let _guard = iroh_test::logging::setup();
        let (relay_map, relay_url, _guard) =
            iroh_net::test_utils::run_relay_server().await.unwrap();

        let ep1 = create_endpoint(&mut rng, relay_map.clone()).await.unwrap();
        let ep2 = create_endpoint(&mut rng, relay_map.clone()).await.unwrap();
        let ep3 = create_endpoint(&mut rng, relay_map.clone()).await.unwrap();
        let addr1 = AddrInfo {
            relay_url: Some(relay_url.clone()),
            direct_addresses: Default::default(),
        };
        let addr2 = AddrInfo {
            relay_url: Some(relay_url.clone()),
            direct_addresses: Default::default(),
        };
        let addr3 = AddrInfo {
            relay_url: Some(relay_url.clone()),
            direct_addresses: Default::default(),
        };

        let go1 = Gossip::from_endpoint(ep1.clone(), Default::default(), &addr1);
        let go2 = Gossip::from_endpoint(ep2.clone(), Default::default(), &addr2);
        let go3 = Gossip::from_endpoint(ep3.clone(), Default::default(), &addr3);
        debug!("peer1 {:?}", ep1.node_id());
        debug!("peer2 {:?}", ep2.node_id());
        debug!("peer3 {:?}", ep3.node_id());
        let pi1 = ep1.node_id();

        let cancel = CancellationToken::new();
        let tasks = [
            spawn(endpoint_loop(ep1.clone(), go1.clone(), cancel.clone())),
            spawn(endpoint_loop(ep2.clone(), go2.clone(), cancel.clone())),
            spawn(endpoint_loop(ep3.clone(), go3.clone(), cancel.clone())),
        ];

        debug!("----- adding peers  ----- ");
        let topic: TopicId = blake3::hash(b"foobar").into();
        // share info that pi1 is on the same relay_node
        let addr1 = NodeAddr::new(pi1).with_relay_url(relay_url);
        ep2.add_node_addr(addr1.clone()).unwrap();
        ep3.add_node_addr(addr1).unwrap();

        debug!("----- joining  ----- ");
        // join the topics and wait for the connection to succeed
        go1.join(topic, vec![]).await.unwrap();
        go2.join(topic, vec![pi1]).await.unwrap().await.unwrap();
        go3.join(topic, vec![pi1]).await.unwrap().await.unwrap();

        let len = 2;

        // subscribe nodes 2 and 3 to the topic
        let mut stream2 = go2.subscribe(topic).await.unwrap();
        let mut stream3 = go3.subscribe(topic).await.unwrap();

        // publish messages on node1
        let pub1 = spawn(async move {
            for i in 0..len {
                let message = format!("hi{}", i);
                info!("go1 broadcast: {message:?}");
                go1.broadcast(topic, message.into_bytes().into())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_micros(1)).await;
            }
        });

        // wait for messages on node2
        let sub2 = spawn(async move {
            let mut recv = vec![];
            loop {
                let ev = stream2.recv().await.unwrap();
                info!("go2 event: {ev:?}");
                if let Event::Received(msg) = ev {
                    recv.push(msg.content);
                }
                if recv.len() == len {
                    return recv;
                }
            }
        });

        // wait for messages on node3
        let sub3 = spawn(async move {
            let mut recv = vec![];
            loop {
                let ev = stream3.recv().await.unwrap();
                info!("go3 event: {ev:?}");
                if let Event::Received(msg) = ev {
                    recv.push(msg.content);
                }
                if recv.len() == len {
                    return recv;
                }
            }
        });

        timeout(Duration::from_secs(10), pub1)
            .await
            .unwrap()
            .unwrap();
        let recv2 = timeout(Duration::from_secs(10), sub2)
            .await
            .unwrap()
            .unwrap();
        let recv3 = timeout(Duration::from_secs(10), sub3)
            .await
            .unwrap()
            .unwrap();

        let expected: Vec<Bytes> = (0..len)
            .map(|i| Bytes::from(format!("hi{i}").into_bytes()))
            .collect();
        assert_eq!(recv2, expected);
        assert_eq!(recv3, expected);

        cancel.cancel();
        for t in tasks {
            timeout(Duration::from_secs(10), t)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }
}
