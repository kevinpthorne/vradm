//! Host-owned bootstrap transaction manager. No PCM transport is enabled here.
//! Drive `poll` with trusted monotonic milliseconds and schedule returned frames.
//! Keep this manager across link resets to retain its 24-hour nonce cache.
//! Process-restart persistence is the host's responsibility and is not supplied.

use crate::security::*;

pub const NONCE_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapMode {
    Fsk100,
    Mcs0,
}

impl BootstrapMode {
    fn delays(self) -> [u64; 4] {
        // Three retransmissions; after the last one wait the largest listed RTO.
        match self {
            Self::Fsk100 => [6000, 9000, 13500, 13500],
            Self::Mcs0 => [7500, 11250, 16875, 16875],
        }
    }
    fn lifetime(self) -> u64 {
        self.delays().iter().sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    Security(SecurityError),
    Busy,
    Replay,
    CacheFull,
    EntropyUnavailable,
    NonceCollision,
    ClockWentBackwards,
    NoPendingTransaction,
    NotEstablished,
}
impl From<SecurityError> for SessionError {
    fn from(error: SecurityError) -> Self {
        Self::Security(error)
    }
}

/// Supply fresh cryptographic entropy. Test implementations may be deterministic.
/// This runs on the host, never in real-time audio callbacks.
pub trait NonceSource {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError>;
}

pub struct OsNonceSource;
impl NonceSource for OsNonceSource {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        let mut nonce = [0; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| SessionError::EntropyUnavailable)?;
        Ok(nonce)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    None,
    Transmit(BootstrapWire),
    Established,
    TimedOut,
}

pub struct SessionContext {
    role: SessionRole,
    keys: SessionKeys,
    initiator_nonce: [u8; 16],
    responder_nonce: [u8; 16],
}
impl SessionContext {
    pub fn role(&self) -> SessionRole {
        self.role
    }
    pub fn keys(&self) -> &SessionKeys {
        &self.keys
    }
}

#[derive(Clone, Copy)]
struct SeenNonce {
    nonce: [u8; 16],
    last_used_ms: u64,
}

struct Initiating {
    pending: PendingBootstrap,
    nonce: [u8; 16],
    wire: BootstrapWire,
    retries: usize,
    next_retry_ms: u64,
    expires_ms: u64,
    mode: BootstrapMode,
}

struct Link {
    tx: ControlTx,
    context: SessionContext,
    rx: ControlRx,
}

/// Single-use handoff of established control ownership to an audio engine.
/// Fields are private to the crate; transfers cannot be constructed or cloned
/// by applications. Queue-full errors return ownership to the caller.
///
/// ```compile_fail
/// use vradm_core::session::SessionTransfer;
/// fn duplicate(session: SessionTransfer) { let second = session.clone(); }
/// ```
pub struct SessionTransfer {
    pub(crate) tx: ControlTx,
    pub(crate) rx: ControlRx,
    pub(crate) role: SessionRole,
    pub(crate) clock_ms: u64,
    pub(crate) idle_confirmation_retries: u8,
}

enum State {
    Idle,
    Initiating(Initiating),
    Responding {
        link: Link,
        reply: BootstrapWire,
        expires_ms: u64,
    },
    Established(Link),
    Transferred(SessionContext),
}

/// Fixed-memory transaction manager with fail-closed cache admission. A full
/// cache never evicts unexpired nonces. Nonces of active/pending sessions remain
/// protected even beyond 24 hours, then for 24 hours after retirement.
/// Role selection uses big-endian uint128 nonce ordering (spec byte order is
/// unspecified), independent of mobile/PBX endpoint type.
pub struct SessionManager<const N: usize = 128> {
    psk: [u8; 16],
    state: State,
    seen: [Option<SeenNonce>; N],
    last_time_ms: u64,
    bootstrap_budget: MacFailureBudget,
}

impl<const N: usize> SessionManager<N> {
    pub fn new(psk: [u8; 16], now_ms: u64) -> Self {
        assert!(N >= 2, "a session needs two nonce cache entries");
        Self {
            psk,
            state: State::Idle,
            seen: [None; N],
            last_time_ms: now_ms,
            bootstrap_budget: MacFailureBudget::new(now_ms),
        }
    }

    fn protected(&self) -> [Option<[u8; 16]>; 2] {
        match &self.state {
            State::Idle => [None, None],
            State::Initiating(tx) => [Some(tx.nonce), None],
            State::Transferred(context) => {
                [Some(context.initiator_nonce), Some(context.responder_nonce)]
            }
            State::Responding { link, .. } | State::Established(link) => [
                Some(link.context.initiator_nonce),
                Some(link.context.responder_nonce),
            ],
        }
    }

    fn is_seen(&self, nonce: &[u8; 16], now_ms: u64) -> bool {
        self.protected().contains(&Some(*nonce))
            || self.seen.iter().flatten().any(|entry| {
                entry.nonce == *nonce
                    && now_ms.saturating_sub(entry.last_used_ms) < NONCE_RETENTION_MS
            })
    }

    fn free_slots(&self, now_ms: u64) -> usize {
        let protected = self.protected();
        self.seen
            .iter()
            .filter(|slot| {
                slot.map_or(true, |entry| {
                    now_ms.saturating_sub(entry.last_used_ms) >= NONCE_RETENTION_MS
                        && !protected.contains(&Some(entry.nonce))
                })
            })
            .count()
    }

    fn remember(&mut self, nonce: [u8; 16], now_ms: u64) {
        let protected = self.protected();
        let slot = self
            .seen
            .iter_mut()
            .find(|slot| {
                slot.map_or(true, |entry| {
                    now_ms.saturating_sub(entry.last_used_ms) >= NONCE_RETENTION_MS
                        && !protected.contains(&Some(entry.nonce))
                })
            })
            .expect("nonce cache capacity preflighted");
        *slot = Some(SeenNonce {
            nonce,
            last_used_ms: now_ms,
        });
    }

    fn retire(&mut self, now_ms: u64) {
        let protected = self.protected();
        for entry in self.seen.iter_mut().flatten() {
            if protected.contains(&Some(entry.nonce)) {
                entry.last_used_ms = now_ms;
            }
        }
        self.state = State::Idle;
    }

    fn advance(&mut self, now_ms: u64) -> Result<bool, SessionError> {
        if now_ms < self.last_time_ms {
            return Err(SessionError::ClockWentBackwards);
        }
        self.last_time_ms = now_ms;
        let expired = match &self.state {
            State::Initiating(tx) => now_ms >= tx.expires_ms,
            State::Responding { expires_ms, .. } => now_ms >= *expires_ms,
            _ => false,
        };
        if expired {
            self.retire(now_ms);
        }
        Ok(expired)
    }

    /// Begin/rekey; returned request must be scheduled for transmission now.
    /// Entropy/cache failure preserves an existing session. A successful rekey
    /// pauses the old session until a new authenticated transaction completes.
    pub fn begin(
        &mut self,
        now_ms: u64,
        mode: BootstrapMode,
        entropy: &mut impl NonceSource,
    ) -> Result<SessionEvent, SessionError> {
        self.advance(now_ms)?;
        if matches!(self.state, State::Initiating(_) | State::Responding { .. }) {
            return Err(SessionError::Busy);
        }
        if self.free_slots(now_ms) < 2 {
            return Err(SessionError::CacheFull);
        }
        let nonce = entropy.nonce()?;
        if self.is_seen(&nonce, now_ms) {
            return Err(SessionError::NonceCollision);
        }
        let pending = PendingBootstrap::new(self.psk, nonce);
        let wire = pending.request()?;
        self.retire(now_ms);
        self.remember(nonce, now_ms);
        self.state = State::Initiating(Initiating {
            pending,
            nonce,
            wire,
            retries: 0,
            next_retry_ms: now_ms.saturating_add(mode.delays()[0]),
            expires_ms: now_ms.saturating_add(mode.lifetime()),
            mode,
        });
        Ok(SessionEvent::Transmit(wire))
    }

    /// Retries preserve the nonce/wire bytes. Late polling emits at most one
    /// retry, never a catch-up burst; the overall lifetime is bounded regardless.
    pub fn poll(&mut self, now_ms: u64) -> Result<SessionEvent, SessionError> {
        if self.advance(now_ms)? {
            return Ok(SessionEvent::TimedOut);
        }
        if let State::Initiating(tx) = &mut self.state {
            if now_ms >= tx.next_retry_ms && tx.retries < 3 {
                tx.retries += 1;
                tx.next_retry_ms = now_ms.saturating_add(tx.mode.delays()[tx.retries]);
                return Ok(SessionEvent::Transmit(tx.wire));
            }
        }
        Ok(SessionEvent::None)
    }

    /// Cumulative bootstrap MAC failures, excluding CRC/format errors, replay
    /// rejection and attempts dropped by the limiter. Persists across reset.
    pub fn bootstrap_mac_failures(&self) -> u32 {
        self.bootstrap_budget.failures()
    }

    /// Process a bootstrap frame off the audio thread. Errors produce no reply.
    /// Request/accept MAC failures share ten burst credits, refilled at 10/s.
    /// CRC/format checks precede throttling; successful tags spend no credit.
    /// A provisional responder resends its cached accept on duplicate requests,
    /// without generating new entropy, changing keys, or extending its deadline.
    pub fn receive(
        &mut self,
        wire: &BootstrapWire,
        now_ms: u64,
        mode: BootstrapMode,
        entropy: &mut impl NonceSource,
    ) -> Result<SessionEvent, SessionError> {
        self.advance(now_ms)?;
        if wire[0] == 0xbf {
            let tx = match &mut self.state {
                State::Initiating(tx) => tx,
                _ => return Err(SessionError::NoPendingTransaction),
            };
            let responder_nonce: [u8; 16] = wire[1..17].try_into().unwrap();
            // One slot was reserved by begin(); no transaction is consumed on
            // resource failure or a reused peer nonce.
            if self.seen.iter().flatten().any(|entry| {
                entry.nonce == responder_nonce
                    && now_ms.saturating_sub(entry.last_used_ms) < NONCE_RETENTION_MS
            }) {
                return Err(SessionError::Replay);
            }
            check_bootstrap_format(wire, 0xbf)?;
            self.bootstrap_budget.admit(now_ms)?;
            let keys = self.bootstrap_budget.record(tx.pending.finish(wire))?;
            let initiator_nonce = tx.nonce;
            self.remember(responder_nonce, now_ms);
            let link = Link {
                tx: ControlTx::new(keys.clone()),
                rx: ControlRx::new(keys.clone(), now_ms),
                context: SessionContext {
                    role: SessionRole::Initiator,
                    keys,
                    initiator_nonce,
                    responder_nonce,
                },
            };
            self.state = State::Established(link);
            return Ok(SessionEvent::Established);
        }
        check_bootstrap_format(wire, 0xbe)?;
        self.bootstrap_budget.admit(now_ms)?;
        let request = self
            .bootstrap_budget
            .record(VerifiedRequest::decode(wire, &self.psk))?;
        let nonce = *request.nonce();
        if let State::Responding { link, reply, .. } = &self.state {
            if link.context.initiator_nonce == nonce {
                return Ok(SessionEvent::Transmit(*reply));
            }
            return Err(SessionError::Busy);
        }
        if let State::Initiating(tx) = &self.state {
            // Lexicographic bytes are unsigned big-endian numeric ordering.
            if tx.nonce >= nonce {
                return Ok(SessionEvent::None);
            }
        }
        if self.is_seen(&nonce, now_ms) {
            return Err(SessionError::Replay);
        }
        if self.free_slots(now_ms) < 2 {
            return Err(SessionError::CacheFull);
        }
        let responder_nonce = entropy.nonce()?;
        if responder_nonce == nonce || self.is_seen(&responder_nonce, now_ms) {
            return Err(SessionError::NonceCollision);
        }
        let (reply, keys) = request.accept(&self.psk, responder_nonce);
        self.retire(now_ms);
        self.remember(nonce, now_ms);
        self.remember(responder_nonce, now_ms);
        let link = Link {
            tx: ControlTx::new(keys.clone()),
            rx: ControlRx::new(keys.clone(), now_ms),
            context: SessionContext {
                role: SessionRole::Responder,
                keys,
                initiator_nonce: nonce,
                responder_nonce,
            },
        };
        self.state = State::Responding {
            link,
            reply,
            expires_ms: now_ms.saturating_add(mode.lifetime()),
        };
        Ok(SessionEvent::Transmit(reply))
    }

    /// Return keys for control verification, including a provisional responder.
    /// Application data must wait for is_established() to become true.
    pub fn context(&mut self, now_ms: u64) -> Result<Option<&SessionContext>, SessionError> {
        self.advance(now_ms)?;
        Ok(match &self.state {
            State::Responding { link, .. } | State::Established(link) => Some(&link.context),
            State::Transferred(context) => Some(context),
            _ => None,
        })
    }

    pub fn is_established(&mut self, now_ms: u64) -> Result<bool, SessionError> {
        self.advance(now_ms)?;
        Ok(matches!(
            self.state,
            State::Established(_) | State::Transferred(_)
        ))
    }

    /// Move established counters exactly once. This manager retains the nonce
    /// history and context for rekey admission, but no longer owns control I/O.
    pub fn take_established(&mut self, now_ms: u64) -> Result<SessionTransfer, SessionError> {
        self.advance(now_ms)?;
        if !matches!(self.state, State::Established(_)) {
            return Err(SessionError::NotEstablished);
        }
        let State::Established(link) = core::mem::replace(&mut self.state, State::Idle) else {
            unreachable!()
        };
        let role = link.context.role;
        self.state = State::Transferred(link.context);
        Ok(SessionTransfer {
            tx: link.tx,
            rx: link.rx,
            role,
            clock_ms: now_ms,
            idle_confirmation_retries: 0,
        })
    }

    /// Sign an outgoing beacon with this session's persistent counter. A
    /// provisional responder cannot transmit data/control beacons yet. Rekey
    /// is required after counter 59,999; the host then calls begin().
    pub fn beacon(
        &mut self,
        current_mcs: u8,
        requested_mcs: u8,
        tx_power: u8,
        now_ms: u64,
    ) -> Result<Beacon, SessionError> {
        self.advance(now_ms)?;
        let State::Established(link) = &mut self.state else {
            return Err(SessionError::NotEstablished);
        };
        Ok(link.tx.beacon(current_mcs, requested_mcs, tx_power)?)
    }

    /// Call only after physical sync/Golay validation. A valid peer beacon
    /// confirms the responder's provisional transaction; invalid input cannot.
    pub fn verify_peer_beacon(&mut self, beacon: Beacon, now_ms: u64) -> Result<u16, SessionError> {
        self.advance(now_ms)?;
        let link = match &mut self.state {
            State::Responding { link, .. } | State::Established(link) => link,
            _ => return Err(SessionError::NoPendingTransaction),
        };
        let counter = link.rx.verify_beacon(beacon, now_ms)?;
        if matches!(self.state, State::Responding { .. }) {
            let State::Responding { link, .. } = core::mem::replace(&mut self.state, State::Idle)
            else {
                unreachable!()
            };
            self.state = State::Established(link);
        }
        Ok(counter)
    }

    /// Trusted reset/outage invalidation. Keeps nonce replay history intact.
    pub fn reset(&mut self, now_ms: u64) -> Result<(), SessionError> {
        self.advance(now_ms)?;
        self.retire(now_ms);
        Ok(())
    }
}
