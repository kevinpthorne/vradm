use crate::session::{SessionRole, SessionTransfer};
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};

use crate::arq::{valid_ip_frame, ArqReceiver, ArqTransmitter};
use crate::c_abi::*;
use crate::framing::CanonicalDataFrame;
use crate::phy::{AudioRingBuffer, PhyReceiver, PhyTransmitter};

/// Leading 224 bits of the standard BLAKE3 hash, as required by SOTP.
pub fn blake3_224(data: &[u8]) -> [u8; 28] {
    blake3::hash(data).as_bytes()[..28].try_into().unwrap()
}

// ============================================================================
// Lock-Free, Wait-Free SPSC Queue with 64-Byte Cache-Line Alignment
// ============================================================================

#[repr(align(64))]
struct CachePadded<T>(pub T);

/// Bounded queue. Use exclusive access locally or split into one endpoint per thread.
///
/// ```compile_fail
/// use vradm_core::engine::SpscQueue;
/// fn shared_producer(queue: &SpscQueue<u8, 4>) { queue.push(1).unwrap(); }
/// ```
/// Dropping the queue drops any remaining items.
pub struct SpscQueue<T, const N: usize> {
    head: CachePadded<AtomicUsize>, // Producer index (Release)
    tail: CachePadded<AtomicUsize>, // Consumer index (Release)
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

unsafe impl<T: Send, const N: usize> Sync for SpscQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Send for SpscQueue<T, N> {}

impl<T, const N: usize> SpscQueue<T, N> {
    pub fn new() -> Self {
        assert!(
            N > 0 && (N & (N - 1)) == 0,
            "Capacity N must be a power of 2"
        );
        let buffer = core::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit()));
        Self {
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
            buffer,
        }
    }

    pub fn push(&mut self, item: T) -> Result<(), T> {
        // Exclusive access implies sole producer ownership.
        unsafe { self.push_shared(item) }
    }

    pub fn pop(&mut self) -> Option<T> {
        unsafe { self.pop_shared() }
    }

    pub fn peek(&mut self) -> Option<&T> {
        unsafe { self.peek_shared() }
    }

    pub fn clear(&mut self) {
        while self.pop().is_some() {}
    }

    /// Borrows the queue until both endpoints are no longer used. Endpoints are
    /// movable across threads, but cannot be cloned or shared across threads.
    pub fn split(&mut self) -> (QueueProducer<'_, T, N>, QueueConsumer<'_, T, N>) {
        (
            QueueProducer {
                queue: self,
                exclusive: core::marker::PhantomData,
            },
            QueueConsumer {
                queue: self,
                exclusive: core::marker::PhantomData,
            },
        )
    }

    /// # Safety
    /// Only the producer may call this; producer calls must not overlap.
    unsafe fn push_shared(&self, item: T) -> Result<(), T> {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= N {
            return Err(item); // Queue full
        }
        let slot_idx = head & (N - 1);
        unsafe {
            (*self.buffer[slot_idx].get()).write(item);
        }
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// # Safety
    /// Only the consumer may call this. No pop/clear may occur while the
    /// returned reference is alive, including on this same thread.
    unsafe fn peek_shared(&self) -> Option<&T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if head == tail {
            return None; // Queue empty
        }
        let slot_idx = tail & (N - 1);
        let item = unsafe { (&*self.buffer[slot_idx].get()).assume_init_ref() };
        Some(item)
    }

    /// # Safety
    /// Only the consumer may call this; no other consumer operation or borrowed
    /// peek result may overlap this call.
    unsafe fn pop_shared(&self) -> Option<T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if head == tail {
            return None; // Queue empty
        }
        let slot_idx = tail & (N - 1);
        let item = unsafe { (*self.buffer[slot_idx].get()).assume_init_read() };
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(item)
    }

    /// # Safety
    /// Requires sole consumer ownership with no outstanding peek references.
    unsafe fn clear_shared(&self) {
        while self.pop_shared().is_some() {}
    }

    /// Approximate occupancy while another thread is active; always at most N.
    pub fn len(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Acquire);
        head.wrapping_sub(tail).min(N)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T, const N: usize> Drop for SpscQueue<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Unique producer obtained from [`SpscQueue::split`].
pub struct QueueProducer<'a, T, const N: usize> {
    queue: &'a SpscQueue<T, N>,
    exclusive: core::marker::PhantomData<core::cell::Cell<()>>,
}

impl<T, const N: usize> QueueProducer<'_, T, N> {
    pub fn push(&mut self, item: T) -> Result<(), T> {
        // Only one producer exists, and &mut serializes its calls.
        unsafe { self.queue.push_shared(item) }
    }
}

/// Unique consumer obtained from [`SpscQueue::split`]. Its peek reference keeps
/// the consumer borrowed, so pop cannot invalidate the reference.
///
/// ```compile_fail
/// use vradm_core::engine::SpscQueue;
/// let mut queue = SpscQueue::<String, 4>::new();
/// queue.push("retained".into()).unwrap();
/// let (_, mut consumer) = queue.split();
/// let item = consumer.peek().unwrap();
/// consumer.pop();
/// println!("{item}");
/// ```
/// A Send-but-not-Sync payload may cross threads, but its consumer must not
/// expose shared peek references across threads.
///
/// ```compile_fail
/// use std::cell::Cell;
/// use vradm_core::engine::QueueConsumer;
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<QueueConsumer<'static, Cell<u32>, 4>>();
/// ```
pub struct QueueConsumer<'a, T, const N: usize> {
    queue: &'a SpscQueue<T, N>,
    exclusive: core::marker::PhantomData<core::cell::Cell<()>>,
}

impl<T, const N: usize> QueueConsumer<'_, T, N> {
    pub fn pop(&mut self) -> Option<T> {
        unsafe { self.queue.pop_shared() }
    }

    pub fn peek(&self) -> Option<&T> {
        // This handle is !Sync even for Send-but-not-Sync T. Its borrow also
        // prevents pop until the returned reference is no longer used.
        unsafe { self.queue.peek_shared() }
    }

    pub fn clear(&mut self) {
        unsafe { self.queue.clear_shared() }
    }
}

// ============================================================================
// IP Packet Slot Definition (MTU 256 + 8 frags * 37 bytes = 296 bytes)
// ============================================================================

pub const MAX_IP_PACKET_LEN: usize = 296;

#[derive(Clone, Copy)]
pub struct PacketSlot {
    generation: u32,
    pub len: u16,
    pub urgent_flush: bool,
    pub best_effort: bool,
    pub data: [u8; MAX_IP_PACKET_LEN],
}

impl Default for PacketSlot {
    fn default() -> Self {
        Self {
            generation: 0,
            len: 0,
            urgent_flush: false,
            best_effort: false,
            data: [0u8; MAX_IP_PACKET_LEN],
        }
    }
}

/// Internal queue envelope; the public C command layout is unchanged.
pub struct QueuedCommand {
    command: vradm_cmd_t,
    generation: u32,
    session: Option<SessionTransfer>,
}

// Project-profile confirmation recovery is paced by rendered 8 kHz samples.
const CONFIRMATION_RETRY_SAMPLES: u64 = 6 * 8000;

struct AudioSecurity {
    session: SessionTransfer,
    capture_samples: u64,
    render_samples: u64,
    next_confirmation_sample: u64,
}

// ============================================================================
// Race-Free Double-Buffered Telemetry Seqlock
// ============================================================================

pub struct TelemetrySeqlock {
    seq: AtomicU32,
    // Retrying a non-atomic struct copy does not undo a Rust data race. Even
    // inactive slots can be reused while a slow reader is copying them.
    slots: [[AtomicU32; 10]; 2],
}

fn telemetry_words(t: vradm_telemetry_t) -> [u32; 10] {
    [
        t.estimated_snr_db.to_bits(),
        u32::from_le_bytes([
            t.active_tx_mcs,
            t.active_rx_mcs,
            t.plcp_carrier_locked,
            t.reserved,
        ]),
        t.security_tamper_detected,
        t.frames_transmitted,
        t.frames_received,
        t.rs_corrected_bytes,
        t.rs_corrected_erasures,
        t.crc_failures,
        t.channel_metric_score.to_bits(),
        t.sample_slip_accum as u32,
    ]
}

fn telemetry_from_words(w: [u32; 10]) -> vradm_telemetry_t {
    let modes = w[1].to_le_bytes();
    vradm_telemetry_t {
        estimated_snr_db: f32::from_bits(w[0]),
        active_tx_mcs: modes[0],
        active_rx_mcs: modes[1],
        plcp_carrier_locked: modes[2],
        reserved: modes[3],
        security_tamper_detected: w[2],
        frames_transmitted: w[3],
        frames_received: w[4],
        rs_corrected_bytes: w[5],
        rs_corrected_erasures: w[6],
        crc_failures: w[7],
        channel_metric_score: f32::from_bits(w[8]),
        sample_slip_accum: w[9] as i32,
    }
}

impl TelemetrySeqlock {
    pub fn new(initial: vradm_telemetry_t) -> Self {
        let words = telemetry_words(initial);
        Self {
            seq: AtomicU32::new(0),
            slots: core::array::from_fn(|_| core::array::from_fn(|i| AtomicU32::new(words[i]))),
        }
    }

    /// Called exclusively by the audio owner (or while the engine is quiescent).
    /// Violating single-writer ownership panics rather than racing or blocking.
    pub fn update<F>(&self, update_fn: F)
    where
        F: FnOnce(&mut vradm_telemetry_t),
    {
        let cur_seq = self.seq.load(Ordering::SeqCst);
        assert_eq!(cur_seq & 1, 0, "concurrent telemetry writers");
        self.seq
            .compare_exchange(
                cur_seq,
                cur_seq.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .expect("concurrent telemetry writers");
        // A panicking Rust callback must not leave diagnostic readers spinning.
        struct Publish<'a> {
            seq: &'a AtomicU32,
            value: u32,
        }
        impl Drop for Publish<'_> {
            fn drop(&mut self) {
                self.seq.store(self.value, Ordering::SeqCst);
            }
        }
        let mut publish = Publish {
            seq: &self.seq,
            value: cur_seq,
        };
        let active_idx = ((cur_seq >> 1) & 1) as usize;
        let mut snapshot = telemetry_from_words(core::array::from_fn(|i| {
            self.slots[active_idx][i].load(Ordering::SeqCst)
        }));
        update_fn(&mut snapshot);
        for (field, word) in self.slots[1 - active_idx]
            .iter()
            .zip(telemetry_words(snapshot))
        {
            field.store(word, Ordering::SeqCst);
        }
        publish.value = cur_seq.wrapping_add(2);
    }

    pub fn read(&self, out: &mut vradm_telemetry_t) {
        loop {
            let seq1 = self.seq.load(Ordering::SeqCst);
            if seq1 & 1 == 0 {
                let active_idx = ((seq1 >> 1) & 1) as usize;
                let words =
                    core::array::from_fn(|i| self.slots[active_idx][i].load(Ordering::SeqCst));
                if seq1 == self.seq.load(Ordering::SeqCst) {
                    *out = telemetry_from_words(words);
                    return;
                }
            }
            core::hint::spin_loop();
        }
    }
}

// ============================================================================
// SOTP Object Transfer States
// ============================================================================

pub struct SotpTxState {
    pub object_id: u32,
    pub redundancy_factor: f32,
    pub payload: Vec<u8>,
    pub hash_blake3_224: [u8; 28],
    pub active: bool,
}

impl Default for SotpTxState {
    fn default() -> Self {
        Self {
            object_id: 0,
            redundancy_factor: 1.0,
            payload: Vec::new(),
            hash_blake3_224: [0u8; 28],
            active: false,
        }
    }
}

pub struct SotpRxState {
    pub state: i32, // VRADM_SOTP_STATE_IDLE, ACCUMULATING, READY
    pub object_id: u32,
    pub total_bytes: u32,
    pub collected_symbols: u32,
    pub required_symbols: u32,
    pub payload: Vec<u8>,
    pub hash_blake3_224: [u8; 28],
}

impl Default for SotpRxState {
    fn default() -> Self {
        Self {
            state: VRADM_SOTP_STATE_IDLE,
            object_id: 0,
            total_bytes: 0,
            collected_symbols: 0,
            required_symbols: 0,
            payload: Vec::new(),
            hash_blake3_224: [0u8; 28],
        }
    }
}

// ============================================================================
// Primary Engine Encapsulation Struct (`vradm_engine`)
// ============================================================================

#[allow(non_camel_case_types)]
pub struct vradm_engine {
    // Configuration & Active Parameters
    pub config: vradm_config_t,
    pub active_tx_mcs: AtomicU8,
    pub active_rx_mcs: AtomicU8,
    pub plcp_carrier_locked: AtomicBool,
    pub is_paused: AtomicBool,
    pub sample_slip_accum: AtomicI32,
    pub beac_seq_counter: AtomicU8,
    pub awaiting_peer_turn: AtomicBool,
    // Audio-thread owned feedback and sample-clock retransmission deadline.
    authentication_required: bool,
    authenticated_generation: AtomicU64,
    audio_security: UnsafeCell<Option<AudioSecurity>>,
    host_generation: AtomicU32,
    audio_generation: AtomicU32,
    ack_pending: AtomicBool,
    peer_wait_samples: AtomicUsize,

    // Lock-Free SPSC Queues
    pub tx_packet_queue: SpscQueue<PacketSlot, 64>, // Host -> Audio
    pub rx_packet_queue: SpscQueue<PacketSlot, 64>, // Audio -> Host
    pub cmd_queue: SpscQueue<QueuedCommand, 32>,    // Host -> Audio

    // Double-Buffered Telemetry Seqlock
    pub telem_seqlock: TelemetrySeqlock,

    // ARQ Link Layer (Single-threaded access on Audio Render Thread)
    pub arq_tx: UnsafeCell<ArqTransmitter>,
    pub arq_rx: UnsafeCell<ArqReceiver>,

    // Physical Layer DSP
    pub phy_tx: UnsafeCell<PhyTransmitter>,
    pub phy_rx: UnsafeCell<PhyReceiver>,

    // Real-Time Audio Circular Ring Buffers
    pub tx_audio_ring: UnsafeCell<AudioRingBuffer>,
    pub rx_audio_ring: UnsafeCell<AudioRingBuffer>,

    // SOTP Management Contexts
    pub sotp_tx: UnsafeCell<SotpTxState>,
    pub sotp_rx: UnsafeCell<SotpRxState>,
}

// All safe shared operations touch atomics only. Safe mutable access is split
// once into disjoint host/audio owners; raw shared operations require unsafe
// caller enforcement of the same ownership contract.
unsafe impl Sync for vradm_engine {}
unsafe impl Send for vradm_engine {}

/// Unique host endpoint. Commands, packets, and object staging are serialized
/// through mutable access to this handle. Move it to the host thread as needed.
///
/// ```compile_fail
/// use vradm_core::engine::HostHandle;
/// fn duplicate(host: HostHandle<'_>) { let second = host.clone(); }
/// ```
pub struct HostHandle<'a> {
    engine: &'a vradm_engine,
}

/// Unique audio endpoint. Both capture and playback use this same handle, so
/// their mutable DSP state cannot be accessed concurrently through safe Rust.
pub struct AudioHandle<'a> {
    engine: &'a vradm_engine,
}

impl HostHandle<'_> {
    /// Queue a single-use session transfer. Success starts a new local packet
    /// generation; queue IP packets afterwards. Prior packets/ARQ state are
    /// discarded at the burst boundary. Failure returns transfer ownership.
    pub fn install_session(
        &mut self,
        session: SessionTransfer,
    ) -> Result<(), (i32, SessionTransfer)> {
        unsafe { self.engine.install_session(session) }
    }

    pub fn authenticated_ready(&self) -> bool {
        self.engine.authenticated_generation.load(Ordering::Acquire)
            == self.engine.host_generation.load(Ordering::Acquire) as u64 + 1
    }

    pub fn submit_cmd(&mut self, cmd: &vradm_cmd_t) -> i32 {
        unsafe { self.engine.submit_cmd(cmd) }
    }

    pub fn write_ip_packet(&mut self, packet: &[u8]) -> i32 {
        unsafe { self.engine.write_ip_packet(packet) }
    }

    pub fn poll_ip_packet(&mut self, out: &mut [u8]) -> i32 {
        unsafe { self.engine.poll_ip_packet(out) }
    }

    pub fn sotp_stage_tx_payload(
        &mut self,
        payload: &[u8],
        redundancy_factor: f32,
        out_object_id: &mut u32,
    ) -> i32 {
        unsafe {
            self.engine
                .sotp_stage_tx_payload(payload, redundancy_factor, out_object_id)
        }
    }

    pub fn sotp_rx_poll(&mut self, collected: &mut u32, required: &mut u32) -> i32 {
        unsafe { self.engine.sotp_rx_poll(collected, required) }
    }

    pub fn sotp_rx_fetch(&mut self, out: &mut [u8], hash: &mut [u8; 28]) -> i32 {
        unsafe { self.engine.sotp_rx_fetch(out, hash) }
    }

    pub fn get_telemetry(&self, out: &mut vradm_telemetry_t) {
        self.engine.get_telemetry(out);
    }

    pub fn get_active_mcs(&self) -> u8 {
        self.engine.get_active_mcs()
    }
}

impl AudioHandle<'_> {
    /// Apply a bounded command batch while external handshake PCM is routed.
    /// Does not generate or decode data audio. Returns false when an existing
    /// engine burst must finish first; it never cuts that burst short.
    pub fn service_commands(&mut self) -> bool {
        unsafe {
            if (*self.engine.tx_audio_ring.get()).available_read() != 0 {
                return false;
            }
            self.engine.process_cmds_on_audio_thread();
        }
        true
    }

    pub fn generate_audio(&mut self, out: &mut [i16]) -> u32 {
        unsafe { self.engine.generate_audio(out) }
    }

    pub fn process_audio(&mut self, samples: &[i16]) {
        unsafe { self.engine.process_audio(samples) }
    }
}

impl vradm_engine {
    /// Obtain one host and one audio owner, borrowing the engine until both
    /// handles are no longer used. No allocation or synchronization is needed.
    /// Safe Rust cannot reset, destroy, or split the engine again while they run.
    ///
    /// ```compile_fail
    /// use vradm_core::engine::vradm_engine;
    /// fn reset_while_active(engine: &mut vradm_engine) {
    ///     let (mut host, _audio) = engine.split();
    ///     engine.reset_exclusive();
    ///     host.write_ip_packet(&[1; 19]);
    /// }
    /// ```
    pub fn split(&mut self) -> (HostHandle<'_>, AudioHandle<'_>) {
        (HostHandle { engine: self }, AudioHandle { engine: self })
    }

    /// Synchronous reset under exclusive ownership, after both handles return.
    pub fn reset_exclusive(&mut self) {
        unsafe { self.reset() }
    }

    /// Opt-in authenticated Rust engine. Requires a session transfer before
    /// data PCM is accepted/emitted. Only verified 8 kHz MCS 2/3 paths are allowed.
    /// The C constructor remains the legacy unauthenticated prototype.
    pub fn new_authenticated(config: vradm_config_t) -> Result<Box<Self>, i32> {
        if config.sample_rate != VRADM_RATE_8K || !matches!(config.startup_mcs, 2 | 3)
            || !config.tx_amplitude.is_finite() || !(0.0..=1.0).contains(&config.tx_amplitude) {
            return Err(VRADM_ERR_INVALID_ARG);
        }
        let mut engine = Box::new(Self::new(config));
        engine.authentication_required = true;
        Ok(engine)
    }

    pub fn new(config: vradm_config_t) -> Self {
        let mut phy_tx = PhyTransmitter::new(config.startup_mcs);
        // Legacy infallible Rust construction fails silent on invalid amplitude.
        // The C and authenticated constructors reject it before reaching here.
        if phy_tx.set_rms_ceiling(config.tx_amplitude).is_err() {
            phy_tx.set_rms_ceiling(0.0).expect("valid silent ceiling");
        }
        let initial_telem = vradm_telemetry_t {
            estimated_snr_db: 25.0,
            active_tx_mcs: config.startup_mcs,
            active_rx_mcs: config.startup_mcs,
            plcp_carrier_locked: 0,
            reserved: 0,
            security_tamper_detected: 0,
            frames_transmitted: 0,
            frames_received: 0,
            rs_corrected_bytes: 0,
            rs_corrected_erasures: 0,
            crc_failures: 0,
            channel_metric_score: 1.0,
            sample_slip_accum: 0,
        };

        Self {
            config,
            active_tx_mcs: AtomicU8::new(config.startup_mcs),
            active_rx_mcs: AtomicU8::new(config.startup_mcs),
            plcp_carrier_locked: AtomicBool::new(false),
            is_paused: AtomicBool::new(false),
            sample_slip_accum: AtomicI32::new(0),
            beac_seq_counter: AtomicU8::new(1),
            awaiting_peer_turn: AtomicBool::new(false),
            authentication_required: false,
            authenticated_generation: AtomicU64::new(0),
            audio_security: UnsafeCell::new(None),
            host_generation: AtomicU32::new(0),
            audio_generation: AtomicU32::new(0),
            ack_pending: AtomicBool::new(false),
            peer_wait_samples: AtomicUsize::new(0),

            tx_packet_queue: SpscQueue::new(),
            rx_packet_queue: SpscQueue::new(),
            cmd_queue: SpscQueue::new(),

            telem_seqlock: TelemetrySeqlock::new(initial_telem),

            arq_tx: UnsafeCell::new(ArqTransmitter::new()),
            arq_rx: UnsafeCell::new(ArqReceiver::new()),

            phy_tx: UnsafeCell::new(phy_tx),
            phy_rx: UnsafeCell::new(PhyReceiver::new()),

            tx_audio_ring: UnsafeCell::new(AudioRingBuffer::new()),
            rx_audio_ring: UnsafeCell::new(AudioRingBuffer::new()),

            sotp_tx: UnsafeCell::new(SotpTxState::default()),
            sotp_rx: UnsafeCell::new(SotpRxState::default()),
        }
    }

    /// # Safety
    /// No other engine operations or outstanding internal references may exist;
    /// all host/audio work must be quiescent.
    pub unsafe fn reset(&self) {
        // Must be called under Quiescence Invariant
        self.tx_packet_queue.clear_shared();
        self.rx_packet_queue.clear_shared();
        self.cmd_queue.clear_shared();

        self.host_generation.store(0, Ordering::Release);
        self.audio_generation.store(0, Ordering::Release);
        self.reset_host_objects();
        self.reset_link_on_audio_thread();
    }

    // Host-only (or quiescent): Vec ownership and deallocation stay off audio.
    unsafe fn reset_host_objects(&self) {
        unsafe {
            *self.sotp_tx.get() = SotpTxState::default();
            *self.sotp_rx.get() = SotpRxState::default();
        }
    }

    // Does not consume host-owned queues or touch host-owned object buffers.
    unsafe fn reset_link_on_audio_thread(&self) {
        *self.audio_security.get() = None;
        self.authenticated_generation.store(0, Ordering::Release);
        unsafe {
            (*self.arq_tx.get()).reset();
            (*self.arq_rx.get()).reset();
            (*self.phy_tx.get()).reset(self.config.startup_mcs);
            (*self.phy_rx.get()).reset();
            (*self.tx_audio_ring.get()).clear();
            (*self.rx_audio_ring.get()).clear();
        }

        self.active_tx_mcs
            .store(self.config.startup_mcs, Ordering::Release);
        self.active_rx_mcs
            .store(self.config.startup_mcs, Ordering::Release);
        self.plcp_carrier_locked.store(false, Ordering::Release);
        self.sample_slip_accum.store(0, Ordering::Release);
        self.beac_seq_counter.store(1, Ordering::Release);
        self.awaiting_peer_turn.store(false, Ordering::Release);
        self.ack_pending.store(false, Ordering::Relaxed);
        self.peer_wait_samples.store(0, Ordering::Relaxed);

        self.telem_seqlock.update(|t| {
            t.active_tx_mcs = self.config.startup_mcs;
            t.active_rx_mcs = self.config.startup_mcs;
            t.plcp_carrier_locked = 0;
            t.security_tamper_detected = 0;
            t.frames_transmitted = 0;
            t.frames_received = 0;
            t.rs_corrected_bytes = 0;
            t.rs_corrected_erasures = 0;
            t.crc_failures = 0;
            t.sample_slip_accum = 0;
        });
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn submit_cmd(&self, cmd: &vradm_cmd_t) -> i32 {
        if cmd.cmd_type == VRADM_CMD_SET_TX_PARAMS
            && (!cmd.param_f32.is_finite() || !(0.0..=1.0).contains(&cmd.param_f32)) {
            return VRADM_ERR_INVALID_ARG;
        }
        if self.authentication_required
            && cmd.cmd_type == VRADM_CMD_REQUEST_MCS
            && !matches!(cmd.param_u32, 2 | 3)
        {
            return VRADM_ERR_INVALID_ARG;
        }
        let mut generation = self.host_generation.load(Ordering::Relaxed);
        if cmd.cmd_type == VRADM_CMD_RESET_SESSION {
            generation = generation.wrapping_add(1);
        }
        if self
            .cmd_queue
            .push_shared(QueuedCommand {
                command: *cmd,
                generation,
                session: None,
            })
            .is_err()
        {
            return VRADM_ERR_QUEUE_FULL;
        }
        if cmd.cmd_type == VRADM_CMD_RESET_SESSION {
            // Publish only after successful enqueue. Later host packets belong
            // to this reset; old RX packets are discarded by the host consumer.
            self.host_generation.store(generation, Ordering::Release);
            self.reset_host_objects();
            for _ in 0..64 {
                let Some(slot) = self.rx_packet_queue.peek_shared() else {
                    break;
                };
                if slot.generation == generation {
                    break;
                }
                self.rx_packet_queue.pop_shared();
            }
        }
        VRADM_OK
    }

    /// # Safety
    /// Requires the sole host owner, serialized with packet/command/SOTP calls.
    /// No reset/destruction/internal-state access may overlap; audio may run.
    pub unsafe fn install_session(
        &self,
        session: SessionTransfer,
    ) -> Result<(), (i32, SessionTransfer)> {
        if !self.authentication_required {
            return Err((VRADM_ERR_STATE, session));
        }
        let generation = self.host_generation.load(Ordering::Relaxed).wrapping_add(1);
        let command = vradm_cmd_t {
            cmd_type: VRADM_CMD_NONE,
            cmd_id: 0,
            param_u32: 0,
            param_i32: 0,
            param_f32: 0.0,
            inline_payload: [0; 12],
        };
        if let Err(queued) = self.cmd_queue.push_shared(QueuedCommand {
            command,
            generation,
            session: Some(session),
        }) {
            return Err((VRADM_ERR_QUEUE_FULL, queued.session.unwrap()));
        }
        self.host_generation.store(generation, Ordering::Release);
        self.reset_host_objects();
        // RX stale generations are discarded by the host's existing polling path.
        Ok(())
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn write_ip_packet(&self, packet: &[u8]) -> i32 {
        if packet.is_empty() || packet.len() > MAX_IP_PACKET_LEN {
            return VRADM_ERR_INVALID_ARG;
        }

        // Fast header inspection for Mosh UDP (ports 60000..60010) and TCP URG
        let mut best_effort = false;
        let mut urgent_flush = false;

        if packet.len() >= 20 && (packet[0] >> 4) == 4 {
            let proto = packet[9];
            if proto == 17 && packet.len() >= 24 {
                // UDP: dst port at bytes 22..24
                let dst_port = u16::from_be_bytes([packet[22], packet[23]]);
                if (60000..=60010).contains(&dst_port) {
                    best_effort = true;
                }
            } else if proto == 6 && packet.len() >= 34 {
                // TCP: check flags at offset ihl * 4 + 13
                let ihl = (packet[0] & 0x0F) as usize * 4;
                if packet.len() >= ihl + 14 {
                    let flags = packet[ihl + 13];
                    // PSH (0x08), URG (0x20), RST (0x04), FIN (0x01)
                    if (flags & 0x2D) != 0 {
                        urgent_flush = true;
                    }
                }
            }
        }

        let mut slot = PacketSlot {
            generation: self.host_generation.load(Ordering::Relaxed),
            len: packet.len() as u16,
            urgent_flush,
            best_effort,
            data: [0u8; MAX_IP_PACKET_LEN],
        };
        slot.data[..packet.len()].copy_from_slice(packet);

        if self.tx_packet_queue.push_shared(slot).is_err() {
            VRADM_ERR_QUEUE_FULL
        } else {
            VRADM_OK
        }
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn poll_ip_packet(&self, out: &mut [u8]) -> i32 {
        let generation = self.host_generation.load(Ordering::Acquire);
        for _ in 0..64 {
            let Some(slot) = self.rx_packet_queue.peek_shared() else {
                return 0;
            };
            if slot.generation != generation {
                self.rx_packet_queue.pop_shared();
                continue;
            }
            if slot.len as usize > out.len() {
                return VRADM_ERR_BUFFER_TOO_SMALL;
            }
            let slot = self
                .rx_packet_queue
                .pop_shared()
                .expect("single host consumer");
            let len = slot.len as usize;
            out[..len].copy_from_slice(&slot.data[..len]);
            return len as i32;
        }
        0
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn sotp_stage_tx_payload(
        &self,
        payload: &[u8],
        redundancy_factor: f32,
        out_object_id: &mut u32,
    ) -> i32 {
        if payload.is_empty() || redundancy_factor < 1.0 {
            return VRADM_ERR_INVALID_ARG;
        }

        let hash = blake3_224(payload);
        let sotp = unsafe { &mut *self.sotp_tx.get() };
        sotp.object_id = sotp.object_id.wrapping_add(1);
        if sotp.object_id == 0 {
            sotp.object_id = 1;
        }
        sotp.redundancy_factor = redundancy_factor;
        sotp.payload = payload.to_vec();
        sotp.hash_blake3_224 = hash;
        sotp.active = true;

        *out_object_id = sotp.object_id;
        VRADM_OK
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn sotp_rx_poll(
        &self,
        out_collected_symbols: &mut u32,
        out_required_symbols: &mut u32,
    ) -> i32 {
        let sotp = unsafe { &*self.sotp_rx.get() };
        *out_collected_symbols = sotp.collected_symbols;
        *out_required_symbols = sotp.required_symbols;
        sotp.state
    }

    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn sotp_rx_fetch(&self, out_buf: &mut [u8], out_hash: &mut [u8]) -> i32 {
        let sotp = unsafe { &mut *self.sotp_rx.get() };
        if sotp.state != VRADM_SOTP_STATE_READY {
            return VRADM_ERR_STATE;
        }
        let len = sotp.payload.len();
        if len > out_buf.len() {
            return VRADM_ERR_BUFFER_TOO_SMALL;
        }

        out_buf[..len].copy_from_slice(&sotp.payload);
        out_hash[..28].copy_from_slice(&sotp.hash_blake3_224);

        // Reset state after fetching
        sotp.state = VRADM_SOTP_STATE_IDLE;
        len as i32
    }

    /// Internal helper for test harness to simulate object arrival
    /// # Safety
    /// Caller must be the sole host owner. Serialize all command, packet, and
    /// SOTP operations; no direct reset, destruction, or internal-state access
    /// may overlap. The sole audio owner may run concurrently.
    pub unsafe fn stage_rx_object_for_test(&self, object_id: u32, data: &[u8]) {
        let sotp = unsafe { &mut *self.sotp_rx.get() };
        sotp.state = VRADM_SOTP_STATE_READY;
        sotp.object_id = object_id;
        sotp.total_bytes = data.len() as u32;
        let symbols = (data.len() + 31) / 32;
        sotp.required_symbols = symbols as u32;
        sotp.collected_symbols = symbols as u32;
        sotp.payload = data.to_vec();
        sotp.hash_blake3_224 = blake3_224(data);
    }

    pub fn get_telemetry(&self, out: &mut vradm_telemetry_t) {
        self.telem_seqlock.read(out);
    }

    pub fn get_active_mcs(&self) -> u8 {
        self.active_tx_mcs.load(Ordering::Relaxed)
    }

    // ========================================================================
    // Real-Time Audio Callback Handlers
    // ========================================================================

    unsafe fn process_cmds_on_audio_thread(&self) {
        // Bound callback work even if the host keeps submitting commands.
        for _ in 0..32 {
            let Some(queued) = self.cmd_queue.pop_shared() else {
                break;
            };
            if let Some(session) = queued.session {
                self.reset_link_on_audio_thread();
                *self.audio_security.get() = Some(AudioSecurity {
                    session,
                    capture_samples: 0,
                    render_samples: 0,
                    next_confirmation_sample: CONFIRMATION_RETRY_SAMPLES,
                });
                self.audio_generation
                    .store(queued.generation, Ordering::Release);
                self.authenticated_generation
                    .store(queued.generation as u64 + 1, Ordering::Release);
                continue;
            }
            let cmd = queued.command;
            match cmd.cmd_type {
                VRADM_CMD_SET_TX_PARAMS => {
                    (*self.phy_tx.get()).set_rms_ceiling(cmd.param_f32)
                        .expect("validated host amplitude command");
                }
                VRADM_CMD_REQUEST_MCS => {
                    if cmd.param_u32 <= 4 {
                        let new_mcs = cmd.param_u32 as u8;
                        self.active_tx_mcs.store(new_mcs, Ordering::Release);
                        self.telem_seqlock.update(|t| t.active_tx_mcs = new_mcs);
                    }
                }
                VRADM_CMD_RESET_SESSION => {
                    self.reset_link_on_audio_thread();
                    self.audio_generation
                        .store(queued.generation, Ordering::Release);
                }
                _ => {}
            }
        }
    }

    /// # Safety
    /// Caller must be the sole audio owner. Serialize both audio callbacks;
    /// no direct reset, destruction, or internal-state access may overlap.
    ///
    /// ```compile_fail
    /// use vradm_core::engine::vradm_engine;
    /// fn shared_audio(engine: &vradm_engine) { engine.generate_audio(&mut [0; 160]); }
    /// ```
    pub unsafe fn generate_audio(&self, out_samples: &mut [i16]) -> u32 {
        if out_samples.is_empty() {
            return 0;
        }
        if unsafe { (*self.tx_audio_ring.get()).available_read() } == 0 {
            self.process_cmds_on_audio_thread();
        }

        if let Some(security) = (*self.audio_security.get()).as_mut() {
            security.render_samples = security
                .render_samples
                .saturating_add(out_samples.len() as u64);
        }

        let tx_ring = unsafe { &mut *self.tx_audio_ring.get() };
        let arq_tx = unsafe { &mut *self.arq_tx.get() };
        let arq_rx = unsafe { &mut *self.arq_rx.get() };
        self.drain_received_packets(arq_rx);
        let phy_tx = unsafe { &mut *self.phy_tx.get() };

        // Finish an existing burst before changing link state or synthesizing another.
        if tx_ring.available_read() > 0 {
            let n = tx_ring.read_samples(out_samples);
            out_samples[n..].fill(0);
            return out_samples.len() as u32;
        }

        if self.authentication_required && (*self.audio_security.get()).is_none() {
            out_samples.fill(0);
            return out_samples.len() as u32;
        }

        // The prototype has no negotiated RTO profile yet. Bound the wait using
        // the longest peer burst plus a one-second margin, measured only after
        // our burst has played out. This is not the adaptive §6.3 RTT estimator.
        if self.awaiting_peer_turn.load(Ordering::Acquire) {
            let remaining = self.peer_wait_samples.load(Ordering::Relaxed);
            if remaining > out_samples.len() {
                self.peer_wait_samples
                    .store(remaining - out_samples.len(), Ordering::Relaxed);
                out_samples.fill(0);
                return out_samples.len() as u32;
            }
            self.awaiting_peer_turn.store(false, Ordering::Release);
        }

        // Discard stale TX slots only as the audio-owned consumer. A future
        // generation waits until its reset command reaches an audio boundary.
        let generation = self.audio_generation.load(Ordering::Relaxed);
        for _ in 0..64 {
            let Some(slot) = self.tx_packet_queue.peek_shared() else {
                break;
            };
            if slot.generation == generation {
                break;
            }
            if generation.wrapping_sub(slot.generation) >= (1 << 31) {
                out_samples.fill(0);
                return out_samples.len() as u32;
            }
            self.tx_packet_queue.pop_shared();
        }

        // Preserve host backpressure until a complete packet fits in the ARQ queue.
        if let Some(slot) = self.tx_packet_queue.peek_shared() {
            let fragments = (slot.len as usize + 36) / 37;
            let queue = if slot.best_effort {
                &arq_tx.pending_be
            } else {
                &arq_tx.pending_reliable
            };
            if queue.len() + fragments <= queue.capacity() {
                if let Some(slot) = self.tx_packet_queue.pop_shared() {
                    arq_tx
                        .enqueue_packet(
                            &slot.data[..slot.len as usize],
                            slot.urgent_flush,
                            slot.best_effort,
                        )
                        .expect("capacity checked before consuming packet");
                }
            }
        }

        let confirmation_due = (*self.audio_security.get())
            .as_ref()
            .map_or(false, |security| {
                security.session.idle_confirmation_retries > 0
                    && security
                        .render_samples
                        .saturating_sub(out_samples.len() as u64)
                        >= security.next_confirmation_sample
            });
        let mut frames = [CanonicalDataFrame::new(); 8];
        let mut n_frames = arq_tx.get_frames_to_transmit_into(8, &mut frames);
        if n_frames == 0 && (self.ack_pending.load(Ordering::Relaxed) || confirmation_due) {
            // §2.1 permits zero-payload canonical feedback. Mark it best-effort
            // so feedback never consumes REL_SEQ or demands an ACK of its own.
            // Authenticated compact-control turns remain a separate milestone.
            frames[0].ctrl = 0x06;
            n_frames = 1;
        }
        if n_frames == 0 {
            out_samples.fill(0);
            return out_samples.len() as u32;
        }

        let cur_mcs = self.active_tx_mcs.load(Ordering::Relaxed);
        let mut needs_ack = false;
        for (i, frame) in frames.iter_mut().enumerate().take(n_frames) {
            frame.ack_base = arq_rx.ack_base;
            frame.ack_map = arq_rx.ack_map;
            frame.ctrl = (frame.ctrl & !0x78) | (cur_mcs << 4);
            if i == n_frames - 1 {
                frame.ctrl |= 0x08;
            }
            needs_ack |= frame.payload_len > 0 && frame.ctrl & 0x04 == 0;
        }
        let pcm_burst = if self.authentication_required {
            let security = (*self.audio_security.get())
                .as_mut()
                .expect("authenticated gate");
            let beacon = match security.session.tx.beacon(cur_mcs, cur_mcs, 0) {
                Ok(beacon) => beacon,
                Err(_) => {
                    *self.audio_security.get() = None;
                    self.authenticated_generation.store(0, Ordering::Release);
                    out_samples.fill(0);
                    return out_samples.len() as u32;
                }
            };
            if confirmation_due {
                // Application traffic can serve as a confirmation probe too.
                // Only actual transmission consumes a retry; never replay an old MAC.
                security.session.idle_confirmation_retries -= 1;
                security.next_confirmation_sample = security
                    .render_samples
                    .saturating_sub(out_samples.len() as u64)
                    .saturating_add(CONFIRMATION_RETRY_SAMPLES);
            }
            let direction = security.session.role == SessionRole::Initiator;
            phy_tx
                .modulate_authenticated(beacon, &frames[..n_frames], direction)
                .expect("authenticated engine supports only MCS 2/3")
        } else {
            let beac_seq = self.beac_seq_counter.fetch_add(1, Ordering::Relaxed);
            phy_tx.modulate_burst(cur_mcs, &frames[..n_frames], beac_seq)
        };
        tx_ring.write_samples(pcm_burst);
        self.ack_pending.store(false, Ordering::Relaxed);
        self.awaiting_peer_turn.store(needs_ack, Ordering::Release);
        let frame_samples = if cur_mcs == 2 { 5200 } else { 1320 };
        self.peer_wait_samples
            .store(4640 + 8 * frame_samples + 8000, Ordering::Relaxed);
        self.telem_seqlock.update(|t| {
            t.frames_transmitted = t.frames_transmitted.wrapping_add(n_frames as u32);
        });
        let n = tx_ring.read_samples(out_samples);
        out_samples[n..].fill(0);
        out_samples.len() as u32
    }

    // Audio-owner only. Held packets can become deliverable without new PCM,
    // so both render and capture callbacks drain when the host frees capacity.
    unsafe fn drain_received_packets(&self, receiver: &mut ArqReceiver) {
        let mut packet_buf = [0; MAX_IP_PACKET_LEN];
        while self.rx_packet_queue.len() < 64 {
            let Some((len, best_effort)) = receiver.poll_packet_into(&mut packet_buf) else {
                break;
            };
            let mut slot = PacketSlot {
                generation: self.audio_generation.load(Ordering::Relaxed),
                len: len as u16,
                urgent_flush: false,
                best_effort,
                data: [0; MAX_IP_PACKET_LEN],
            };
            slot.data[..len].copy_from_slice(&packet_buf[..len]);
            // Single producer: the host can only free slots during this call.
            assert!(self.rx_packet_queue.push_shared(slot).is_ok());
        }
    }

    /// # Safety
    /// Caller must be the sole audio owner. Serialize both audio callbacks;
    /// no direct reset, destruction, or internal-state access may overlap.
    pub unsafe fn process_audio(&self, in_samples: &[i16]) {
        if unsafe { (*self.tx_audio_ring.get()).available_read() } == 0 {
            self.process_cmds_on_audio_thread();
        }
        let phy_rx = unsafe { &mut *self.phy_rx.get() };
        if self.authentication_required && (*self.audio_security.get()).is_none() {
            phy_rx.reset();
            return;
        }
        let arq_tx = unsafe { &mut *self.arq_tx.get() };
        let arq_rx = unsafe { &mut *self.arq_rx.get() };

        self.drain_received_packets(arq_rx);
        phy_rx.ingest_samples(in_samples);

        let mut frames = [CanonicalDataFrame::new(); 8];
        let n_frames = if self.authentication_required {
            let security = (*self.audio_security.get())
                .as_mut()
                .expect("authenticated gate");
            security.capture_samples = security
                .capture_samples
                .saturating_add(in_samples.len() as u64);
            let now_ms = security
                .session
                .clock_ms
                .saturating_add(security.capture_samples / 8);
            let direction = security.session.role == SessionRole::Responder;
            let before = security.session.rx.mac_failures();
            let count = phy_rx.process_with_verifier(&mut frames, direction, &mut |beacon| {
                let verified = matches!(beacon.current_mcs, 2 | 3)
                    && security.session.rx.verify_beacon(beacon, now_ms).is_ok();
                if verified {
                    security.session.idle_confirmation_retries = 0;
                }
                verified
            });
            let failures = security.session.rx.mac_failures().saturating_sub(before);
            if failures > 0 {
                self.telem_seqlock.update(|t| {
                    t.security_tamper_detected =
                        t.security_tamper_detected.saturating_add(failures);
                });
            }
            count
        } else {
            phy_rx.process(&mut frames)
        };

        if n_frames > 0 {
            self.plcp_carrier_locked.store(true, Ordering::Release);

            for frame in frames.iter().take(n_frames) {
                self.telem_seqlock.update(|t| {
                    t.frames_received = t.frames_received.wrapping_add(1);
                    t.plcp_carrier_locked = 1;
                });

                if !valid_ip_frame(frame) {
                    continue;
                }

                // Ingest piggybacked ACK into transmitter
                arq_tx.on_ack_received(frame.ack_base, frame.ack_map);

                // If peer frame sets TDD_YIELD, channel ownership returns to this node
                if (frame.ctrl & 0x08) != 0 {
                    self.awaiting_peer_turn.store(false, Ordering::Release);
                }

                // ACK duplicates too: the previous feedback may have been lost.
                // Empty feedback and best-effort datagrams never solicit an ACK.
                if frame.payload_len > 0 && frame.ctrl & 0x04 == 0 {
                    self.ack_pending.store(true, Ordering::Relaxed);
                }

                // The audio owner is the only queue producer; the host can
                // only free capacity concurrently. If full, leave reassembly
                // and ACK state unchanged so reliable data can be retried.
                // Feedback above still runs, avoiding reverse-link deadlock.
                if frame.payload_len > 0 && self.rx_packet_queue.len() >= 64 {
                    continue;
                }

                arq_rx.ingest_frame(frame);
                self.drain_received_packets(arq_rx);
            }
        }
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blake3_test_vector_empty() {
        // Standard BLAKE3-256 for empty input is:
        // af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
        // BLAKE3-224 is the leading 28 bytes
        let digest = blake3_224(b"");
        use std::fmt::Write;
        let mut hex = String::with_capacity(digest.len() * 2);
        for b in &digest {
            write!(&mut hex, "{:02x}", b).unwrap();
        }
        assert_eq!(
            hex,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93ca"
        );
    }

    #[test]
    fn test_spsc_queue_push_pop_overflow() {
        let mut queue: SpscQueue<u32, 4> = SpscQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);

        assert_eq!(queue.push(10), Ok(()));
        assert_eq!(queue.push(20), Ok(()));
        assert_eq!(queue.push(30), Ok(()));
        assert_eq!(queue.push(40), Ok(()));
        // Queue full: capacity is 4
        assert_eq!(queue.push(50), Err(50));
        assert_eq!(queue.len(), 4);

        assert_eq!(queue.pop(), Some(10));
        assert_eq!(queue.pop(), Some(20));
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.push(60), Ok(()));
        assert_eq!(queue.pop(), Some(30));
        assert_eq!(queue.pop(), Some(40));
        assert_eq!(queue.pop(), Some(60));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_indices_wrap_usize_max() {
        let mut queue = SpscQueue::<String, 4>::new();
        queue.head.0.store(usize::MAX - 1, Ordering::Relaxed);
        queue.tail.0.store(usize::MAX - 1, Ordering::Relaxed);
        for i in 0..4 {
            queue.push(i.to_string()).unwrap();
        }
        assert_eq!(queue.len(), 4);
        assert_eq!(queue.push("full".into()), Err("full".into()));
        for i in 0..4 {
            assert_eq!(queue.pop(), Some(i.to_string()));
        }
        assert_eq!(queue.head.0.load(Ordering::Relaxed), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_telemetry_seqlock_consistency() {
        let initial = vradm_telemetry_t {
            estimated_snr_db: 20.0,
            active_tx_mcs: 2,
            active_rx_mcs: 2,
            plcp_carrier_locked: 0,
            reserved: 0,
            security_tamper_detected: 0,
            frames_transmitted: 100,
            frames_received: 95,
            rs_corrected_bytes: 4,
            rs_corrected_erasures: 2,
            crc_failures: 1,
            channel_metric_score: 0.98,
            sample_slip_accum: 0,
        };
        let seqlock = TelemetrySeqlock::new(initial);

        let mut read_telem = initial;
        seqlock.read(&mut read_telem);
        assert_eq!(read_telem.frames_transmitted, 100);

        seqlock.update(|t| {
            t.frames_transmitted += 50;
            t.rs_corrected_bytes += 10;
        });

        seqlock.read(&mut read_telem);
        assert_eq!(read_telem.frames_transmitted, 150);
        assert_eq!(read_telem.rs_corrected_bytes, 14);
    }

    #[test]
    fn test_engine_lifecycle_and_ip_queues() {
        let config = vradm_config_t {
            sample_rate: VRADM_RATE_8K,
            startup_mcs: VRADM_MCS_2,
            auto_rate_adaptation: 0,
            reserved: [0; 2],
            tx_amplitude: 0.3535,
            reserved2: [0; 4],
            psk_key: [0x42; 16],
        };
        let engine = vradm_engine::new(config);

        let packet = b"test_ip_packet_payload";
        let res = unsafe { engine.write_ip_packet(packet) };
        assert_eq!(res, VRADM_OK);

        // Queue has 1 packet
        assert_eq!(engine.tx_packet_queue.len(), 1);

        unsafe { engine.reset() };
        assert_eq!(engine.tx_packet_queue.len(), 0);
    }
}
