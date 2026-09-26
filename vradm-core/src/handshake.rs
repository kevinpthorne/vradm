//! Host-owned, opt-in 8 kHz Barker bootstrap PCM coordinator.
//!
//! All methods run on a host worker, not an audio callback: they can allocate,
//! compute waveforms and request OS entropy. Feed bounded, correctly paced PCM
//! from an adapter and render outbound PCM without mixing it with engine data.
//! After Ready, drain playback before transferring the session to the engine.
//! See docs/HANDSHAKE_COORDINATOR.md for routing and loss-recovery limitations.

use crate::bootstrap_phy::{
    BarkerBootstrapReceiver, BarkerBootstrapTransmitter, BARKER_BOOTSTRAP_SAMPLES,
};
use crate::framing::CanonicalDataFrame;
use crate::phy::{PhyReceiver, PhyTransmitter};
use crate::security::BootstrapWire;
use crate::session::{
    BootstrapMode, NonceSource, SessionError, SessionEvent, SessionManager, SessionRole,
    SessionTransfer,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePhase {
    Listening,
    Negotiating,
    Ready,
    Transferred,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    UnsupportedMcs,
    Busy,
    NotReady,
    Session(SessionError),
}
impl From<SessionError> for HandshakeError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

enum Transmission {
    Bootstrap(BootstrapWire),
    Confirmation,
}

/// One bounded transaction and one deferred transmission. Incoming malformed,
/// forged, replayed and rate-limited frames are silently discarded and counted.
/// Entropy/cache errors are returned to the host. Keep this object across resets
/// to preserve nonce history and the bootstrap verification failure budget.
pub struct HandshakeCoordinator<const N: usize = 128> {
    manager: SessionManager<N>,
    bootstrap_rx: BarkerBootstrapReceiver,
    confirmation_rx: PhyReceiver,
    confirmation_tx: PhyTransmitter,
    mcs: u8,
    phase: HandshakePhase,
    waiting_confirmation: bool,
    pending: Option<Transmission>,
    playback: Vec<i16>,
    position: usize,
    rejected_frames: u32,
}

impl<const N: usize> HandshakeCoordinator<N> {
    pub fn new(psk: [u8; 16], mcs: u8, now_ms: u64) -> Result<Self, HandshakeError> {
        if !matches!(mcs, 2 | 3) {
            return Err(HandshakeError::UnsupportedMcs);
        }
        Ok(Self {
            manager: SessionManager::new(psk, now_ms),
            bootstrap_rx: BarkerBootstrapReceiver::new(),
            confirmation_rx: PhyReceiver::new(),
            confirmation_tx: PhyTransmitter::new(mcs),
            mcs,
            phase: HandshakePhase::Listening,
            waiting_confirmation: false,
            pending: None,
            playback: Vec::with_capacity(BARKER_BOOTSTRAP_SAMPLES),
            position: 0,
            rejected_frames: 0,
        })
    }

    pub fn phase(&self) -> HandshakePhase {
        self.phase
    }
    pub fn rejected_bootstrap_frames(&self) -> u32 {
        self.rejected_frames
    }
    pub fn bootstrap_mac_failures(&self) -> u32 {
        self.manager.bootstrap_mac_failures()
    }

    /// Drop partial physical acquisition after a capture gap; preserve session
    /// keys, replay history, retry deadlines and already scheduled playback.
    pub fn discard_partial_capture(&mut self) {
        self.bootstrap_rx.reset();
        self.confirmation_rx.reset();
    }

    fn clear_media(&mut self) {
        self.bootstrap_rx.reset();
        self.confirmation_rx.reset();
        self.pending = None;
        self.playback.clear();
        self.position = 0;
        self.waiting_confirmation = false;
    }

    /// Explicit host reset; the adapter must also discard queued old PCM and
    /// close/reset any installed data engine before beginning another handshake.
    pub fn reset(&mut self, now_ms: u64) -> Result<(), HandshakeError> {
        self.manager.reset(now_ms)?;
        self.clear_media();
        self.phase = HandshakePhase::Listening;
        Ok(())
    }

    pub fn begin(
        &mut self,
        now_ms: u64,
        entropy: &mut impl NonceSource,
    ) -> Result<(), HandshakeError> {
        if !matches!(
            self.phase,
            HandshakePhase::Listening | HandshakePhase::TimedOut
        ) {
            return Err(HandshakeError::Busy);
        }
        let event = self.manager.begin(now_ms, BootstrapMode::Fsk100, entropy)?;
        self.clear_media();
        self.phase = HandshakePhase::Negotiating;
        self.schedule(event);
        Ok(())
    }

    fn schedule(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Transmit(wire) => {
                self.phase = HandshakePhase::Negotiating;
                // Repeated accepts/retries coalesce into one deferred job and
                // never restart or truncate the waveform currently playing.
                self.pending = Some(Transmission::Bootstrap(wire));
                if wire[0] == 0xbf && !self.waiting_confirmation {
                    self.waiting_confirmation = true;
                    self.confirmation_rx.reset();
                }
            }
            SessionEvent::Established => {
                self.waiting_confirmation = false;
                self.pending = Some(Transmission::Confirmation);
            }
            SessionEvent::TimedOut => {
                self.clear_media();
                self.phase = HandshakePhase::TimedOut;
            }
            SessionEvent::None => {}
        }
    }

    /// Service retries and deadlines even when no PCM arrives. Time must be
    /// trusted monotonic milliseconds; a backward clock returns an error.
    pub fn poll(&mut self, now_ms: u64) -> Result<HandshakePhase, HandshakeError> {
        let event = self.manager.poll(now_ms)?;
        self.schedule(event);
        Ok(self.phase)
    }

    /// Host-worker capture input. Internally chunked so oversized input cannot
    /// overflow the bounded PHY receiver. No application payload is delivered
    /// here, including payload accompanying the first confirming beacon.
    pub fn process_pcm(
        &mut self,
        input: &[i16],
        now_ms: u64,
        entropy: &mut impl NonceSource,
    ) -> Result<(), HandshakeError> {
        self.poll(now_ms)?;
        if matches!(
            self.phase,
            HandshakePhase::Ready | HandshakePhase::Transferred | HandshakePhase::TimedOut
        ) {
            return Ok(());
        }
        for chunk in input.chunks(160) {
            if self.waiting_confirmation {
                self.confirmation_rx.ingest_samples(chunk);
                let mut frames = [CanonicalDataFrame::new(); 8];
                let manager = &mut self.manager;
                let mut confirmed = false;
                self.confirmation_rx
                    .process_with_verifier(&mut frames, true, &mut |beacon| {
                        let valid = matches!(beacon.current_mcs, 2 | 3)
                            && manager.verify_peer_beacon(beacon, now_ms).is_ok();
                        confirmed |= valid;
                        valid
                    });
                if confirmed {
                    self.waiting_confirmation = false;
                    // Any deferred accept is now obsolete. Finish active playback
                    // before exposing readiness, preserving the burst boundary.
                    self.pending = None;
                    if self.position == self.playback.len() {
                        self.phase = HandshakePhase::Ready;
                    }
                    return Ok(());
                }
            }
            let mut offset = 0;
            while offset < chunk.len() {
                let progress = self.bootstrap_rx.push(&chunk[offset..]);
                offset += progress.consumed;
                if let Some(decoded) = progress.frame {
                    let result = match decoded {
                        Ok(wire) => {
                            self.manager
                                .receive(&wire, now_ms, BootstrapMode::Fsk100, entropy)
                        }
                        Err(_) => {
                            self.rejected_frames = self.rejected_frames.saturating_add(1);
                            continue;
                        }
                    };
                    match result {
                        Ok(event) => self.schedule(event),
                        Err(
                            error @ (SessionError::EntropyUnavailable
                            | SessionError::NonceCollision
                            | SessionError::CacheFull),
                        ) => return Err(error.into()),
                        Err(_) => self.rejected_frames = self.rejected_frames.saturating_add(1),
                    }
                }
            }
        }
        Ok(())
    }

    /// Host-worker playback rendering. Returns non-padding sample count; fills
    /// the remainder with silence. At most one waveform is rendered per call.
    /// Ready means rendered, not physically played: the adapter must drain its
    /// playback queue before switching to the data engine.
    pub fn render_pcm(&mut self, out: &mut [i16], now_ms: u64) -> Result<usize, HandshakeError> {
        out.fill(0);
        self.poll(now_ms)?;
        if out.is_empty() {
            return Ok(0);
        }
        if self.position == self.playback.len() {
            if let Some(job) = self.pending.take() {
                self.playback.clear();
                self.position = 0;
                match job {
                    Transmission::Bootstrap(wire) => {
                        self.playback.resize(BARKER_BOOTSTRAP_SAMPLES, 0);
                        BarkerBootstrapTransmitter::new(wire).render(&mut self.playback);
                    }
                    Transmission::Confirmation => {
                        let beacon = self.manager.beacon(self.mcs, self.mcs, 0, now_ms)?;
                        let mut frame = CanonicalDataFrame::new();
                        frame.ctrl = (self.mcs << 4) | 0x0e; // Empty best-effort, yield.
                        let pcm = self
                            .confirmation_tx
                            .modulate_authenticated(beacon, &[frame], true)
                            .expect("validated MCS and one canonical confirmation frame");
                        self.playback.extend_from_slice(pcm);
                    }
                }
            }
        }
        let count = out.len().min(self.playback.len() - self.position);
        out[..count].copy_from_slice(&self.playback[self.position..self.position + count]);
        self.position += count;
        if self.position == self.playback.len()
            && self.pending.is_none()
            && !self.waiting_confirmation
            && self.manager.is_established(now_ms)?
        {
            self.phase = if self.phase == HandshakePhase::Transferred {
                HandshakePhase::Transferred
            } else {
                HandshakePhase::Ready
            };
        }
        Ok(count)
    }

    /// One-time handoff. Queue-full engine admission returns this same transfer
    /// for host retry; do not construct new counter owners or repeat this call.
    pub fn take_established(&mut self, now_ms: u64) -> Result<SessionTransfer, HandshakeError> {
        self.poll(now_ms)?;
        if self.phase != HandshakePhase::Ready {
            return Err(HandshakeError::NotReady);
        }
        let mut session = self.manager.take_established(now_ms)?;
        if session.role == SessionRole::Initiator {
            // Explicit project-profile recovery, not a new bootstrap wire frame.
            session.idle_confirmation_retries = 3;
        }
        self.phase = HandshakePhase::Transferred;
        Ok(session)
    }
}
