//! Integrated opt-in authenticated 8 kHz MCS2/3 endpoint lifecycle.
//! Host owns entropy/handshake pumping; audio owns both callbacks. Session
//! transfers and bridge/engine routing are internal. See docs/ENDPOINT.md.
use crate::{
    c_abi::*,
    engine::{vradm_engine, AudioHandle, HostHandle},
    handshake::{HandshakeCoordinator, HandshakePhase},
    handshake_bridge::{
        BridgeAudio, HandshakeBridge, HandshakeWorker, PlaybackFence, PlaybackProgress, WorkerError,
    },
    session::{NonceSource, OsNonceSource},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    Engine(i32),
    Worker(WorkerError),
    AlreadySplit,
    ClockWentBackwards,
}
impl From<WorkerError> for EndpointError {
    fn from(error: WorkerError) -> Self {
        Self::Worker(error)
    }
}

/// Split once for the endpoint lifetime. Keep the host handle alive to preserve
/// its reset-persistent nonce cache. Reset/rekey through EndpointHost, not by
/// constructing another endpoint. Dropping the endpoint discards that cache.
pub struct Endpoint<const N: usize = 128> {
    engine: Box<vradm_engine>,
    bridge: HandshakeBridge,
    coordinator: Option<HandshakeCoordinator<N>>,
    initial_time: u64,
}
impl<const N: usize> Endpoint<N> {
    pub fn new(config: vradm_config_t, now_ms: u64) -> Result<Self, EndpointError> {
        // Explicit negotiated upshift is available, but the automatic channel
        // metric estimator/adaptation policy is not implemented.
        if config.auto_rate_adaptation != 0 {
            return Err(EndpointError::Engine(VRADM_ERR_INVALID_ARG));
        }
        let engine = vradm_engine::new_ccf_endpoint(config).map_err(EndpointError::Engine)?;
        let coordinator = HandshakeCoordinator::new(config.psk_key, config.startup_mcs, now_ms)
            .map_err(|e| EndpointError::Worker(WorkerError::Handshake(e)))?;
        Ok(Self {
            engine,
            bridge: HandshakeBridge::new(),
            coordinator: Some(coordinator),
            initial_time: now_ms,
        })
    }

    pub fn split(
        &mut self,
    ) -> Result<(EndpointHost<'_, N, OsNonceSource>, EndpointAudio<'_>), EndpointError> {
        self.split_with_entropy(OsNonceSource)
    }

    /// Entropy injection for deterministic harnesses or a platform RNG. Sources
    /// used in production must satisfy NonceSource's freshness requirements.
    pub fn split_with_entropy<S: NonceSource>(
        &mut self,
        entropy: S,
    ) -> Result<(EndpointHost<'_, N, S>, EndpointAudio<'_>), EndpointError> {
        let coordinator = self.coordinator.take().ok_or(EndpointError::AlreadySplit)?;
        let (host, audio) = self.engine.split();
        let (bridge_host, bridge_audio) = self.bridge.split();
        Ok((
            EndpointHost {
                engine: host,
                worker: HandshakeWorker::new(coordinator, bridge_host),
                entropy,
                last_time: self.initial_time,
            },
            EndpointAudio {
                engine: audio,
                bridge: bridge_audio,
            },
        ))
    }
}

pub struct EndpointHost<'a, const N: usize, S: NonceSource = OsNonceSource> {
    engine: HostHandle<'a>,
    worker: HandshakeWorker<'a, N>,
    entropy: S,
    last_time: u64,
}
impl<const N: usize, S: NonceSource> EndpointHost<'_, N, S> {
    fn advance(&mut self, now_ms: u64) -> Result<(), EndpointError> {
        if now_ms < self.last_time {
            return Err(EndpointError::ClockWentBackwards);
        }
        self.last_time = now_ms;
        Ok(())
    }
    pub fn phase(&self) -> HandshakePhase {
        self.worker.phase()
    }
    /// Local engine installation, not a mutual readiness or device link claim.
    pub fn ready(&self) -> bool {
        self.phase() == HandshakePhase::Transferred && self.engine.authenticated_ready()
    }
    pub fn begin(&mut self, now_ms: u64) -> Result<(), EndpointError> {
        self.advance(now_ms)?;
        self.worker.begin(now_ms, &mut self.entropy)?;
        Ok(())
    }
    /// Host-worker only; may allocate and request entropy. Installation is
    /// automatic after the audio/device fence acknowledgment. Queue pressure
    /// retains the transfer and is retried on the next pump.
    pub fn pump(&mut self, now_ms: u64) -> Result<bool, EndpointError> {
        self.advance(now_ms)?;
        self.worker.pump(now_ms, &mut self.entropy)?;
        match self.worker.install_when_drained(&mut self.engine, now_ms) {
            Ok(_) | Err(WorkerError::Engine(VRADM_ERR_QUEUE_FULL)) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(self.ready())
    }
    /// Stop admission immediately and reset both engine and coordinator. A full
    /// command queue leaves the endpoint intact; retry after callbacks run.
    /// Already rendered/device-queued PCM cannot be retracted. Buffered engine
    /// PCM finishes before the new handshake; callers must keep rendering.
    pub fn reset(&mut self, now_ms: u64) -> Result<(), EndpointError> {
        self.advance(now_ms)?;
        let result = self
            .engine
            .submit_cmd(&command(VRADM_CMD_RESET_SESSION, 0.0));
        if result != VRADM_OK {
            return Err(EndpointError::Engine(result));
        }
        // Only possible failure is clock regression, preflighted above.
        self.worker.reset(now_ms)?;
        Ok(())
    }
    pub fn write_ip_packet(&mut self, packet: &[u8]) -> i32 {
        if !self.ready() {
            return VRADM_ERR_STATE;
        }
        self.engine.write_ip_packet(packet)
    }
    pub fn poll_ip_packet(&mut self, out: &mut [u8]) -> i32 {
        if !self.ready() {
            return VRADM_ERR_STATE;
        }
        self.engine.poll_ip_packet(out)
    }
    /// Queue a drained-window MCS2→3 negotiation. Success means queued, not
    /// committed; inspect rate_change_status and telemetry as callbacks run.
    pub fn request_upshift(&mut self, target: u8) -> i32 {
        if !self.ready() { return VRADM_ERR_STATE; }
        self.engine.request_upshift(target)
    }
    /// Return TX to MCS2 at the next burst boundary, without a peer commit.
    /// Requires explicit trusted M < 0.60. Also cancels an in-progress upshift
    /// while already at MCS2. Queued/in-flight reliable data and cooldown survive.
    /// Previously rendered/device-queued audio cannot be retracted.
    pub fn emergency_downshift(&mut self, metric: f32) -> i32 {
        if !self.ready() { return VRADM_ERR_STATE; }
        self.engine.emergency_downshift(metric)
    }
    /// Trusted local channel metric M in [0,1]. No estimate is inferred from
    /// prototype telemetry. Reset clears it; M >= 0.85 permits peer upshifts.
    pub fn set_channel_metric(&mut self, metric: f32) -> i32 {
        if !self.ready() { return VRADM_ERR_STATE; }
        self.engine.set_channel_metric(metric)
    }
    pub fn rate_change_status(&self) -> crate::engine::RateChangeStatus {
        self.engine.rate_change_status()
    }
    pub fn set_tx_amplitude(&mut self, ceiling: f32) -> i32 {
        self.engine
            .submit_cmd(&command(VRADM_CMD_SET_TX_PARAMS, ceiling))
    }
    /// Authenticated compact ACKs sent/received since the last session reset.
    pub fn ccf_counts(&self) -> (u64, u64) { self.engine.ccf_counts() }
    pub fn get_telemetry(&self, out: &mut vradm_telemetry_t) {
        self.engine.get_telemetry(out);
    }
}
fn command(kind: u32, amplitude: f32) -> vradm_cmd_t {
    vradm_cmd_t {
        cmd_type: kind,
        cmd_id: 0,
        param_u32: 0,
        param_i32: 0,
        param_f32: amplitude,
        inline_payload: [0; 12],
    }
}

/// Bound to its own endpoint's engine/bridge; callers cannot accidentally route
/// another engine's PCM or counters through it. Serialize both audio callbacks.
pub struct EndpointAudio<'a> {
    engine: AudioHandle<'a>,
    bridge: BridgeAudio<'a>,
}
impl<'a> EndpointAudio<'a> {
    pub fn process_audio(&mut self, input: &[i16]) -> usize {
        self.bridge.process_audio(&mut self.engine, input)
    }
    pub fn generate_audio(&mut self, out: &mut [i16]) -> PlaybackProgress {
        // Reset can disable routing during a buffered data burst. Finish it so
        // its queued reset can run; otherwise handshake routing would strand
        // the old ring forever and prevent session installation.
        if !self.bridge.routes_to_engine() && !self.engine.service_commands() {
            self.engine.generate_audio(out);
            return PlaybackProgress {
                samples: out.len(),
                awaiting_device_drain: false,
            };
        }
        self.bridge.generate_audio(&mut self.engine, out)
    }
    pub fn playback_fence(&mut self) -> Option<PlaybackFence<'a>> {
        self.bridge.playback_fence()
    }
    pub fn acknowledge_played(&mut self, fence: PlaybackFence<'_>) -> bool {
        self.bridge.acknowledge_played(fence)
    }
}
