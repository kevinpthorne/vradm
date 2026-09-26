//! Bounded worker/audio PCM bridge for the opt-in handshake coordinator.
//! Audio callbacks only copy samples and operate SPSC queues/atomics. A playback
//! fence must be explicitly acknowledged after the device drains before handoff.

use crate::engine::{AudioHandle, HostHandle, QueueConsumer, QueueProducer, SpscQueue};
use crate::handshake::{HandshakeCoordinator, HandshakeError, HandshakePhase};
use crate::session::{NonceSource, SessionTransfer};
use core::sync::atomic::{AtomicU64, Ordering};

pub const PCM_BLOCK_SAMPLES: usize = 160;
pub const PCM_QUEUE_BLOCKS: usize = 32;

#[derive(Clone, Copy)]
struct Block {
    generation: u64,
    offset: u64,
    len: usize,
    samples: [i16; PCM_BLOCK_SAMPLES],
}

enum Playback {
    Samples(Block),
    Fence(u64),
}

struct Shared {
    generation: AtomicU64,
    drained: AtomicU64,
    engine_route: AtomicU64,
}

/// Split once into unique endpoints. Re-splitting discards old queued media.
pub struct HandshakeBridge {
    capture: SpscQueue<Block, PCM_QUEUE_BLOCKS>,
    playback: SpscQueue<Playback, PCM_QUEUE_BLOCKS>,
    shared: Shared,
}
impl Default for HandshakeBridge {
    fn default() -> Self {
        Self::new()
    }
}
impl HandshakeBridge {
    pub fn new() -> Self {
        Self {
            capture: SpscQueue::new(),
            playback: SpscQueue::new(),
            shared: Shared {
                generation: AtomicU64::new(0),
                drained: AtomicU64::new(0),
                engine_route: AtomicU64::new(0),
            },
        }
    }

    pub fn split(&mut self) -> (BridgeHost<'_>, BridgeAudio<'_>) {
        self.capture.clear();
        self.playback.clear();
        let generation = self
            .shared
            .generation
            .load(Ordering::Relaxed)
            .checked_add(1)
            .expect("bridge generation exhausted");
        self.shared.generation.store(generation, Ordering::Release);
        let (capture_tx, capture_rx) = self.capture.split();
        let (playback_tx, playback_rx) = self.playback.split();
        (
            BridgeHost {
                capture: capture_rx,
                playback: playback_tx,
                shared: &self.shared,
                generation,
                capture_end: 0,
                sealed: false,
            },
            BridgeAudio {
                capture: capture_tx,
                playback: playback_rx,
                shared: &self.shared,
                generation,
                capture_offset: 0,
                current: None,
                position: 0,
                fence: None,
            },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    Full,
    Sealed,
    InvalidLength,
}

pub struct CapturedPcm {
    pub samples: [i16; PCM_BLOCK_SAMPLES],
    pub len: usize,
    pub discontinuity: bool,
}

/// Unique worker endpoint. Reset changes generations; it never consumes the
/// audio-owned playback queue or touches the callback's partial block.
pub struct BridgeHost<'a> {
    capture: QueueConsumer<'a, Block, PCM_QUEUE_BLOCKS>,
    playback: QueueProducer<'a, Playback, PCM_QUEUE_BLOCKS>,
    shared: &'a Shared,
    generation: u64,
    capture_end: u64,
    sealed: bool,
}
impl BridgeHost<'_> {
    pub fn reset(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("bridge generation exhausted");
        self.capture_end = 0;
        self.sealed = false;
        self.shared
            .generation
            .store(self.generation, Ordering::Release);
    }

    pub fn pop_capture(&mut self) -> Option<CapturedPcm> {
        for _ in 0..PCM_QUEUE_BLOCKS {
            let block = self.capture.pop()?;
            if block.generation != self.generation {
                continue;
            }
            let discontinuity = block.offset != self.capture_end;
            self.capture_end = block.offset.wrapping_add(block.len as u64);
            return Some(CapturedPcm {
                samples: block.samples,
                len: block.len,
                discontinuity,
            });
        }
        None
    }

    /// Atomic whole-block admission; queue-full never consumes input.
    pub fn push_playback(&mut self, samples: &[i16]) -> Result<(), BridgeError> {
        if self.sealed {
            return Err(BridgeError::Sealed);
        }
        if samples.is_empty() || samples.len() > PCM_BLOCK_SAMPLES {
            return Err(BridgeError::InvalidLength);
        }
        let mut block = Block {
            generation: self.generation,
            offset: 0,
            len: samples.len(),
            samples: [0; PCM_BLOCK_SAMPLES],
        };
        block.samples[..samples.len()].copy_from_slice(samples);
        self.playback
            .push(Playback::Samples(block))
            .map_err(|_| BridgeError::Full)
    }

    /// Enqueue a fence after all handshake PCM. Retry on Full; idempotent after
    /// admission. No later playback is accepted until reset.
    pub fn seal_playback(&mut self) -> Result<(), BridgeError> {
        if !self.sealed {
            self.playback
                .push(Playback::Fence(self.generation))
                .map_err(|_| BridgeError::Full)?;
            self.sealed = true;
        }
        Ok(())
    }

    pub fn playback_drained(&self) -> bool {
        self.sealed && self.shared.drained.load(Ordering::Acquire) == self.generation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackProgress {
    /// Non-padding samples copied to the output slice.
    pub samples: usize,
    /// Fence reached. The device adapter must drain its downstream buffers
    /// before calling acknowledge_played; reaching a fence is not proof of play.
    pub awaiting_device_drain: bool,
}

/// Identifies a particular bridge generation's playback fence. Retain this
/// token with the device drain request; an old completion cannot acknowledge
/// a newer handshake or a different bridge. The borrow keeps its bridge alive.
#[derive(Clone, Copy)]
pub struct PlaybackFence<'a> {
    shared: &'a Shared,
    generation: u64,
}

/// Unique audio endpoint; serialize capture, render and device acknowledgment.
/// Handles cannot be cloned or shared for concurrent callbacks.
pub struct BridgeAudio<'a> {
    capture: QueueProducer<'a, Block, PCM_QUEUE_BLOCKS>,
    playback: QueueConsumer<'a, Playback, PCM_QUEUE_BLOCKS>,
    shared: &'a Shared,
    generation: u64,
    capture_offset: u64,
    current: Option<Block>,
    position: usize,
    fence: Option<u64>,
}
impl<'a> BridgeAudio<'a> {
    fn synchronize(&mut self) {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.generation = generation;
            self.capture_offset = 0;
            self.current = None;
            self.position = 0;
            self.fence = None;
        }
    }

    /// Returns dropped sample count. Dropped blocks advance the stream offset,
    /// so the worker detects the gap before joining later samples to a decoder.
    pub fn capture(&mut self, input: &[i16]) -> usize {
        self.synchronize();
        let mut dropped = 0;
        for chunk in input.chunks(PCM_BLOCK_SAMPLES) {
            let mut block = Block {
                generation: self.generation,
                offset: self.capture_offset,
                len: chunk.len(),
                samples: [0; PCM_BLOCK_SAMPLES],
            };
            block.samples[..chunk.len()].copy_from_slice(chunk);
            self.capture_offset = self.capture_offset.wrapping_add(chunk.len() as u64);
            if self.capture.push(block).is_err() {
                dropped += chunk.len();
            }
        }
        dropped
    }

    /// Report a device capture discontinuity even when no samples were supplied.
    pub fn capture_discontinuity(&mut self) {
        self.synchronize();
        self.capture_offset = self.capture_offset.wrapping_add(1);
    }

    /// Copies across block boundaries; silence on underflow and at a fence.
    /// A fence encountered after a full output slice is observed next callback.
    pub fn render(&mut self, out: &mut [i16]) -> PlaybackProgress {
        self.synchronize();
        out.fill(0);
        let mut written = 0;
        let mut stale = 0;
        while written < out.len() && self.fence.is_none() {
            if let Some(block) = &self.current {
                let count = (block.len - self.position).min(out.len() - written);
                out[written..written + count]
                    .copy_from_slice(&block.samples[self.position..self.position + count]);
                written += count;
                self.position += count;
                if self.position == block.len {
                    self.current = None;
                }
                continue;
            }
            let Some(next) = self.playback.pop() else {
                break;
            };
            match next {
                Playback::Samples(block) if block.generation == self.generation => {
                    self.current = Some(block);
                    self.position = 0;
                }
                Playback::Fence(generation) if generation == self.generation => {
                    self.fence = Some(generation);
                }
                _ => {
                    stale += 1;
                    if stale >= PCM_QUEUE_BLOCKS {
                        break;
                    }
                }
            }
        }
        PlaybackProgress {
            samples: written,
            awaiting_device_drain: self.fence.is_some(),
        }
    }

    /// Capture this token when scheduling device drain, not when its delayed
    /// completion arrives. A reset invalidates previously issued tokens.
    pub fn playback_fence(&mut self) -> Option<PlaybackFence<'a>> {
        self.synchronize();
        self.fence.map(|generation| PlaybackFence {
            shared: self.shared,
            generation,
        })
    }

    /// Call only once the device has consumed PCM preceding the supplied fence.
    /// Rejects stale-generation and wrong-bridge device completions.
    pub fn acknowledge_played(&mut self, fence: PlaybackFence<'_>) -> bool {
        self.synchronize();
        if core::ptr::eq(fence.shared, self.shared)
            && fence.generation == self.generation
            && self.fence == Some(fence.generation)
        {
            self.shared
                .drained
                .store(fence.generation, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn routes_to_engine(&mut self) -> bool {
        self.synchronize();
        self.shared.engine_route.load(Ordering::Acquire) == self.generation
    }

    /// Route to the supplied authenticated engine only after worker installation
    /// succeeds. Both wrappers must always use the same engine as the worker.
    pub fn process_audio(&mut self, engine: &mut AudioHandle<'_>, input: &[i16]) -> usize {
        if self.routes_to_engine() {
            engine.process_audio(input);
            0
        } else {
            engine.service_commands();
            self.capture(input)
        }
    }

    pub fn generate_audio(
        &mut self,
        engine: &mut AudioHandle<'_>,
        out: &mut [i16],
    ) -> PlaybackProgress {
        if self.routes_to_engine() {
            engine.generate_audio(out);
            PlaybackProgress {
                samples: out.len(),
                awaiting_device_drain: false,
            }
        } else {
            engine.service_commands();
            self.render(out)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerError {
    Handshake(HandshakeError),
    Engine(i32),
}
impl From<HandshakeError> for WorkerError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

/// Host-worker pump. Owns entropy-dependent coordination and retains a rejected
/// session transfer on engine queue saturation. Callback code owns only BridgeAudio.
pub struct HandshakeWorker<'a, const N: usize = 128> {
    coordinator: HandshakeCoordinator<N>,
    bridge: BridgeHost<'a>,
    pending_pcm: [i16; PCM_BLOCK_SAMPLES],
    pending_len: usize,
    transfer: Option<SessionTransfer>,
    timed_out: bool,
}
impl<'a, const N: usize> HandshakeWorker<'a, N> {
    pub fn new(coordinator: HandshakeCoordinator<N>, bridge: BridgeHost<'a>) -> Self {
        Self {
            coordinator,
            bridge,
            pending_pcm: [0; PCM_BLOCK_SAMPLES],
            pending_len: 0,
            transfer: None,
            timed_out: false,
        }
    }
    pub fn phase(&self) -> HandshakePhase {
        self.coordinator.phase()
    }
    pub fn playback_drained(&self) -> bool {
        self.bridge.playback_drained()
    }

    pub fn begin(
        &mut self,
        now_ms: u64,
        entropy: &mut impl NonceSource,
    ) -> Result<(), WorkerError> {
        self.coordinator.begin(now_ms, entropy)?;
        self.timed_out = false;
        Ok(())
    }

    /// Also reset/close the associated engine and clear device PCM before rekey.
    /// The bridge generation prevents queued old PCM and drain acknowledgments
    /// from crossing this reset, but cannot retract buffers already in a device.
    pub fn reset(&mut self, now_ms: u64) -> Result<(), WorkerError> {
        self.coordinator.reset(now_ms)?;
        self.bridge.reset();
        self.pending_len = 0;
        self.transfer = None;
        self.timed_out = false;
        Ok(())
    }

    pub fn pump(
        &mut self,
        now_ms: u64,
        entropy: &mut impl NonceSource,
    ) -> Result<HandshakePhase, WorkerError> {
        self.coordinator.poll(now_ms)?;
        if self.phase() == HandshakePhase::Transferred {
            return Ok(self.phase());
        }
        if self.phase() == HandshakePhase::TimedOut {
            if !self.timed_out {
                self.bridge.reset();
                self.pending_len = 0;
                self.timed_out = true;
            }
            return Ok(self.phase());
        }
        for _ in 0..PCM_QUEUE_BLOCKS {
            let Some(block) = self.bridge.pop_capture() else {
                break;
            };
            if block.discontinuity {
                self.coordinator.discard_partial_capture();
            }
            self.coordinator
                .process_pcm(&block.samples[..block.len], now_ms, entropy)?;
        }
        // Bounded pre-rendering: at most the queue capacity plus one retained
        // block. Rendering never advances again until that retained block fits.
        for _ in 0..=PCM_QUEUE_BLOCKS {
            if self.pending_len > 0 {
                match self
                    .bridge
                    .push_playback(&self.pending_pcm[..self.pending_len])
                {
                    Ok(()) => self.pending_len = 0,
                    Err(BridgeError::Full) => return Ok(self.phase()),
                    Err(_) => unreachable!("worker owns playback sealing and block bounds"),
                }
            }
            if self.phase() == HandshakePhase::Ready {
                let _ = self.bridge.seal_playback(); // Full is retried on next pump.
                break;
            }
            self.pending_len = self.coordinator.render_pcm(&mut self.pending_pcm, now_ms)?;
            if self.pending_len == 0 {
                break;
            }
        }
        Ok(self.phase())
    }

    /// Install once after explicit device drain. Queue-full errors retain the
    /// counter owner for retry. Success publishes routing only after enqueueing
    /// installation; the engine audio callback then applies it before PCM I/O.
    pub fn install_when_drained(
        &mut self,
        engine: &mut HostHandle<'_>,
        now_ms: u64,
    ) -> Result<bool, WorkerError> {
        self.coordinator.poll(now_ms)?;
        if self.bridge.shared.engine_route.load(Ordering::Acquire) == self.bridge.generation {
            return Ok(true);
        }
        if !self.bridge.playback_drained() {
            return Ok(false);
        }
        if self.transfer.is_none() {
            self.transfer = Some(self.coordinator.take_established(now_ms)?);
        }
        match engine.install_session(self.transfer.take().expect("transfer retained")) {
            Ok(()) => {
                self.bridge
                    .shared
                    .engine_route
                    .store(self.bridge.generation, Ordering::Release);
                Ok(true)
            }
            Err((code, session)) => {
                self.transfer = Some(session);
                Err(WorkerError::Engine(code))
            }
        }
    }
}
