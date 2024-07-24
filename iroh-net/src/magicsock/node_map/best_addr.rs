//! The [`BestAddr`] is the currently active best address for UDP sends.

use std::net::SocketAddr;

use iroh_metrics::inc;
use tracing::{debug, info};

use crate::magicsock::metrics::Metrics as MagicsockMetrics;
use crate::util::time::{Duration, Instant};

/// How long we trust a UDP address as the exclusive path (without using relay) without having heard a Pong reply.
const TRUST_UDP_ADDR_DURATION: Duration = Duration::from_millis(6500);

#[derive(Debug, Default)]
pub(super) struct BestAddr(Option<BestAddrInner>);

#[derive(Debug)]
struct BestAddrInner {
    addr: AddrLatency,
    trust_until: Option<Instant>,
    confirmed_at: Instant,
}

impl BestAddrInner {
    fn is_trusted(&self, now: Instant) -> bool {
        self.trust_until
            .map(|trust_until| trust_until >= now)
            .unwrap_or(false)
    }

    fn addr(&self) -> SocketAddr {
        self.addr.addr
    }
}

#[derive(Debug)]
pub(super) enum Source {
    ReceivedPong,
    BestCandidate,
    Udp,
}

impl Source {
    fn trust_until(&self, from: Instant) -> Instant {
        match self {
            Source::ReceivedPong => from + TRUST_UDP_ADDR_DURATION,
            // TODO: Fix time
            Source::BestCandidate => from + Duration::from_secs(60 * 60),
            Source::Udp => from + TRUST_UDP_ADDR_DURATION,
        }
    }
}

#[derive(Debug)]
pub(super) enum State<'a> {
    Valid(&'a AddrLatency),
    Outdated(&'a AddrLatency),
    Empty,
}

#[derive(Debug, Clone, Copy)]
pub enum ClearReason {
    Reset,
    Inactive,
    PongTimeout,
    MatchesOurLocalAddr,
}

impl BestAddr {
    #[cfg(test)]
    pub fn from_parts(
        addr: SocketAddr,
        latency: Duration,
        confirmed_at: Instant,
        trust_until: Instant,
    ) -> Self {
        let inner = BestAddrInner {
            addr: AddrLatency { addr, latency },
            confirmed_at,
            trust_until: Some(trust_until),
        };
        Self(Some(inner))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Unconditionally clears the best address.
    pub fn clear(&mut self, reason: ClearReason, has_relay: bool) {
        let old = self.0.take();
        if let Some(old_addr) = old.as_ref().map(BestAddrInner::addr) {
            info!(?reason, ?has_relay, %old_addr, "clearing best_addr");
            // no longer relying on the direct connection
            inc!(MagicsockMetrics, num_direct_conns_removed);
            if has_relay {
                // we are now relying on the relay connection, add a relay conn
                inc!(MagicsockMetrics, num_relay_conns_added);
            }
        }
    }

    /// Clears the best address if equal to `addr`.
    pub fn clear_if_equals(&mut self, addr: SocketAddr, reason: ClearReason, has_relay: bool) {
        if self.addr() == Some(addr) {
            self.clear(reason, has_relay)
        }
    }

    pub fn clear_trust(&mut self, why: &'static str) {
        if let Some(state) = self.0.as_mut() {
            info!(
                %why,
                prev_trust_until = ?state.trust_until,
                "clearing best_addr trust",
            );
            state.trust_until = None;
        }
    }

    pub fn insert_if_better_or_reconfirm(
        &mut self,
        addr: SocketAddr,
        latency: Duration,
        source: Source,
        confirmed_at: Instant,
        has_relay: bool,
    ) {
        match self.0.as_mut() {
            None => {
                self.insert(addr, latency, source, confirmed_at, has_relay);
            }
            Some(state) => {
                let candidate = AddrLatency { addr, latency };
                if !state.is_trusted(confirmed_at) || candidate.is_better_than(&state.addr) {
                    self.insert(addr, latency, source, confirmed_at, has_relay);
                } else if state.addr.addr == addr {
                    state.confirmed_at = confirmed_at;
                    state.trust_until = Some(source.trust_until(confirmed_at));
                }
            }
        }
    }

    /// Reset the expiry, if the passed in addr matches the currently used one.
    pub fn reconfirm_if_used(&mut self, addr: SocketAddr, source: Source, confirmed_at: Instant) {
        if let Some(state) = self.0.as_mut() {
            if state.addr.addr == addr {
                state.confirmed_at = confirmed_at;
                state.trust_until = Some(source.trust_until(confirmed_at));
            }
        }
    }

    fn insert(
        &mut self,
        addr: SocketAddr,
        latency: Duration,
        source: Source,
        confirmed_at: Instant,
        has_relay: bool,
    ) {
        let trust_until = source.trust_until(confirmed_at);

        if self
            .0
            .as_ref()
            .map(|prev| prev.addr.addr == addr)
            .unwrap_or_default()
        {
            debug!(
                %addr,
                latency = ?latency,
                trust_for = ?trust_until.duration_since(Instant::now()),
               "re-selecting direct path for node"
            );
        } else {
            info!(
               %addr,
               latency = ?latency,
               trust_for = ?trust_until.duration_since(Instant::now()),
               "selecting new direct path for node"
            );
        }
        let was_empty = self.is_empty();
        let inner = BestAddrInner {
            addr: AddrLatency { addr, latency },
            trust_until: Some(trust_until),
            confirmed_at,
        };
        self.0 = Some(inner);
        if was_empty && has_relay {
            // we now have a direct connection, adjust direct connection count
            inc!(MagicsockMetrics, num_direct_conns_added);
            if has_relay {
                // we no longer rely on the relay connection, decrease the relay connection
                // count
                inc!(MagicsockMetrics, num_relay_conns_removed);
            }
        }
    }

    pub fn state(&self, now: Instant) -> State {
        match &self.0 {
            None => State::Empty,
            Some(state) => match state.trust_until {
                Some(expiry) if now < expiry => State::Valid(&state.addr),
                Some(_) | None => State::Outdated(&state.addr),
            },
        }
    }

    pub fn addr(&self) -> Option<SocketAddr> {
        self.0.as_ref().map(BestAddrInner::addr)
    }
}

/// A `SocketAddr` with an associated latency.
#[derive(Debug, Clone)]
pub struct AddrLatency {
    pub addr: SocketAddr,
    pub latency: Duration,
}

impl AddrLatency {
    /// Reports whether `self` is a better addr to use than `other`.
    fn is_better_than(&self, other: &Self) -> bool {
        if self.addr == other.addr {
            return false;
        }
        if self.addr.is_ipv6() && other.addr.is_ipv4() {
            // Prefer IPv6 for being a bit more robust, as long as
            // the latencies are roughly equivalent.
            if self.latency / 10 * 9 < other.latency {
                return true;
            }
        } else if self.addr.is_ipv4() && other.addr.is_ipv6() && other.is_better_than(self) {
            return false;
        }
        self.latency < other.latency
    }
}
