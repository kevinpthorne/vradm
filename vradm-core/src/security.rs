//! Session-security building blocks for SPEC §4.0–4.1.
//!
//! The opt-in authenticated Rust PCM engine uses these persistent counters; the
//! legacy C engine remains unauthenticated. SessionManager supplies fresh CSPRNG
//! nonces and replay admission; hosts must still coordinate bootstrap retries
//! and installation of established sessions.
//! MAC8 is only the spec's lightweight control integrity filter, not strong
//! application authentication. Session roles are independent of phone/PBX roles.
//!
//! Wire convention: the spec does not state SipHash64 byte order; bootstrap
//! tags here use little-endian bytes (the SipHash reference convention).

use crate::crc::payload_crc16;
use crate::framing::CompactControlFrame;
use siphasher::sip::SipHasher24;
use subtle::ConstantTimeEq;

const REQUEST: u8 = 0xbe;
const ACCEPT: u8 = 0xbf;
pub const REKEY_COUNTER: u16 = 60_000;
pub type BootstrapWire = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityError {
    ChannelIntegrity,
    Malformed,
    BadMac,
    EpochMismatch,
    NoPendingRequest,
    Replay,
    /// Untrusted sequence cannot be inferred; drop it without resetting state.
    CounterInference,
    /// Trusted outage invalidated this context; fresh bootstrap is required.
    ResyncRequired,
    RekeyRequired,
    RateLimited,
    OutstandingRequest,
    UnexpectedControl,
    ClockWentBackwards,
}

fn siphash(key: &[u8; 16], input: &[u8]) -> u64 {
    SipHasher24::new_with_key(key).hash(input)
}

/// Derived secrets. Deliberately does not implement Debug or Copy.
#[derive(Clone)]
pub struct SessionKeys {
    epoch: u32,
    control: [u8; 16],
}

impl SessionKeys {
    pub fn derive(psk: &[u8; 16], initiator_nonce: &[u8; 16], responder_nonce: &[u8; 16]) -> Self {
        let mut material = [0; 48];
        material[..16].copy_from_slice(psk);
        material[16..32].copy_from_slice(initiator_nonce);
        material[32..].copy_from_slice(responder_nonce);
        let session = blake3::derive_key("VRADM-v3.8-SESSION-EPOCH", &material);
        let epoch = u32::from_le_bytes(session[..4].try_into().unwrap());
        let mut material = [0; 20];
        material[..16].copy_from_slice(psk);
        material[16..].copy_from_slice(&epoch.to_le_bytes());
        let control = blake3::derive_key("VRADM-v3.8-CONTROL-MAC", &material);
        Self {
            epoch,
            control: control[..16].try_into().unwrap(),
        }
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    fn beacon_mac(&self, counter: u16, beacon: &Beacon) -> u8 {
        let mut input = [0; 10];
        input[..4].copy_from_slice(&self.epoch.to_le_bytes());
        input[4..6].copy_from_slice(&counter.to_le_bytes());
        input[6..].copy_from_slice(&[
            beacon.current_mcs,
            beacon.requested_mcs,
            beacon.tx_power,
            beacon.sequence,
        ]);
        siphash(&self.control, &input) as u8
    }

    fn ccf_mac(&self, counter: u16, frame: &CompactControlFrame) -> u8 {
        let mut input = [0; 11];
        input[..4].copy_from_slice(&self.epoch.to_le_bytes());
        input[4..6].copy_from_slice(&counter.to_le_bytes());
        input[6..9].copy_from_slice(&[frame.ccf_ctrl, frame.ack_base, frame.ack_map]);
        let crc = payload_crc16(&input[6..9]);
        input[9..].copy_from_slice(&crc.to_be_bytes());
        siphash(&self.control, &input) as u8
    }

    /// Sign a CCF acknowledging a verified peer burst's full 16-bit counter.
    pub fn sign_ccf(
        &self,
        peer_counter: u16,
        mut frame: CompactControlFrame,
    ) -> Result<[u8; 16], SecurityError> {
        if peer_counter >= REKEY_COUNTER {
            return Err(SecurityError::RekeyRequired);
        }
        frame.ccf_mac = self.ccf_mac(peer_counter, &frame);
        Ok(frame.encode())
    }
}

fn bootstrap_wire(
    kind: u8,
    nonce: &[u8; 16],
    epoch: u32,
    psk: &[u8; 16],
    initiator: Option<&[u8; 16]>,
) -> BootstrapWire {
    let mut wire = [0; 32];
    wire[0] = kind;
    wire[1..17].copy_from_slice(nonce);
    wire[17..21].copy_from_slice(&epoch.to_le_bytes());
    let tag = bootstrap_mac(&wire, psk, initiator);
    wire[21..29].copy_from_slice(&tag);
    let crc = payload_crc16(&wire[..29]);
    wire[29..31].copy_from_slice(&crc.to_be_bytes());
    wire
}

fn bootstrap_mac(wire: &BootstrapWire, psk: &[u8; 16], initiator: Option<&[u8; 16]>) -> [u8; 8] {
    let hash = if let Some(nonce) = initiator {
        let mut input = [0; 37];
        input[..16].copy_from_slice(nonce);
        input[16..].copy_from_slice(&wire[..21]);
        siphash(psk, &input)
    } else {
        siphash(psk, &wire[..21])
    };
    hash.to_le_bytes()
}

/// Cheap channel/format checks, before admitting a bootstrap MAC attempt.
pub(crate) fn check_bootstrap_format(wire: &BootstrapWire, kind: u8) -> Result<(), SecurityError> {
    if payload_crc16(&wire[..29]) != u16::from_be_bytes([wire[29], wire[30]]) {
        return Err(SecurityError::ChannelIntegrity);
    }
    if wire[0] != kind || wire[31] != 0 || (kind == REQUEST && wire[17..21] != [0; 4]) {
        return Err(SecurityError::Malformed);
    }
    Ok(())
}

fn check_bootstrap(
    wire: &BootstrapWire,
    kind: u8,
    psk: &[u8; 16],
    initiator: Option<&[u8; 16]>,
) -> Result<(), SecurityError> {
    check_bootstrap_format(wire, kind)?;
    if !bool::from(bootstrap_mac(wire, psk, initiator).ct_eq(&wire[21..29])) {
        return Err(SecurityError::BadMac);
    }
    Ok(())
}

/// One outstanding initiator transaction. Failed responses preserve it; a
/// successful response consumes its nonce so it cannot be accepted twice.
/// This object owns a PSK copy and must be kept off diagnostic output.
pub struct PendingBootstrap {
    psk: [u8; 16],
    nonce: Option<[u8; 16]>,
}

impl PendingBootstrap {
    /// `nonce` must be newly generated by a host CSPRNG for this transaction.
    pub fn new(psk: [u8; 16], nonce: [u8; 16]) -> Self {
        Self {
            psk,
            nonce: Some(nonce),
        }
    }

    /// Retries resend the same request, not a newly generated nonce.
    pub fn request(&self) -> Result<BootstrapWire, SecurityError> {
        let nonce = self.nonce.as_ref().ok_or(SecurityError::NoPendingRequest)?;
        Ok(bootstrap_wire(REQUEST, nonce, 0, &self.psk, None))
    }

    pub fn finish(&mut self, wire: &BootstrapWire) -> Result<SessionKeys, SecurityError> {
        let nonce = self.nonce.as_ref().ok_or(SecurityError::NoPendingRequest)?;
        check_bootstrap(wire, ACCEPT, &self.psk, Some(nonce))?;
        let responder = wire[1..17].try_into().unwrap();
        let keys = SessionKeys::derive(&self.psk, nonce, &responder);
        if keys.epoch.to_le_bytes() != wire[17..21] {
            return Err(SecurityError::EpochMismatch);
        }
        self.nonce = None;
        Ok(keys)
    }
}

/// MAC-verified request, NOT replay-admitted. The session manager must check
/// this nonce against the active transaction and 24-hour cache before replying.
pub struct VerifiedRequest {
    nonce: [u8; 16],
}

impl VerifiedRequest {
    pub fn decode(wire: &BootstrapWire, psk: &[u8; 16]) -> Result<Self, SecurityError> {
        check_bootstrap(wire, REQUEST, psk, None)?;
        Ok(Self {
            nonce: wire[1..17].try_into().unwrap(),
        })
    }

    pub fn nonce(&self) -> &[u8; 16] {
        &self.nonce
    }

    /// Only call after replay admission, supplying a fresh host CSPRNG nonce.
    pub fn accept(
        &self,
        psk: &[u8; 16],
        responder_nonce: [u8; 16],
    ) -> (BootstrapWire, SessionKeys) {
        let keys = SessionKeys::derive(psk, &self.nonce, &responder_nonce);
        let wire = bootstrap_wire(ACCEPT, &responder_nonce, keys.epoch, psk, Some(&self.nonce));
        (wire, keys)
    }
}

/// Decoded PLCP fields. Physical sync and Golay checks must precede verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beacon {
    pub current_mcs: u8,
    pub requested_mcs: u8,
    pub tx_power: u8,
    pub sequence: u8,
    pub mac: u8,
}

impl Beacon {
    fn validate(&self) -> Result<(), SecurityError> {
        if self.current_mcs > 4 || self.requested_mcs > 4 || self.tx_power > 3 {
            Err(SecurityError::Malformed)
        } else {
            Ok(())
        }
    }
}

/// Supported CCF command classes from SPEC §2.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ControlCommand {
    StandaloneAck = 1,
    McsCommitAck = 2,
    TddGrant = 3,
}

/// Expected authenticated response to one locally emitted beacon. The deadline
/// is a trusted absolute millisecond time, chosen by the future media scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRequest {
    pub current_mcs: u8,
    pub target_mcs: u8,
    pub tx_power: u8,
    pub command: ControlCommand,
    pub yield_turn: bool,
    pub deadline_ms: u64,
}

#[derive(Clone, Copy)]
struct PendingControl {
    counter: u16,
    expected_ctrl: u8,
    deadline_ms: u64,
    semantic_ack_base: Option<u8>,
}

/// Verified ACK fields may be ingested for every response. Apply control state
/// changes only when apply_semantics is true. A repeated MCS commit retains the
/// first authenticated switchover boundary even if its ACK information changes.
#[derive(Debug)]
pub struct ControlResponse {
    pub frame: CompactControlFrame,
    pub apply_semantics: bool,
    pub commit_sequence: Option<u8>,
}

/// Per-direction transmit counters. It cannot emit counter 60,000 or wrap.
pub struct ControlTx {
    keys: SessionKeys,
    next: u16,
    pending: Option<PendingControl>,
    last_control_time_ms: u64,
}

impl ControlTx {
    pub fn new(keys: SessionKeys) -> Self {
        Self {
            keys,
            next: 0,
            pending: None,
            last_control_time_ms: 0,
        }
    }

    pub fn control_outstanding(&self) -> bool {
        self.pending
            .map_or(false, |pending| pending.semantic_ack_base.is_none())
    }

    pub fn last_counter(&self) -> Option<u16> {
        self.next.checked_sub(1)
    }

    /// Expire a request (including its duplicate-response context) at its exact
    /// deadline. Returns true once. No new counter is consumed by timeout.
    pub fn expire_control(&mut self, now_ms: u64) -> Result<bool, SecurityError> {
        if now_ms < self.last_control_time_ms {
            return Err(SecurityError::ClockWentBackwards);
        }
        self.last_control_time_ms = now_ms;
        if self
            .pending
            .map_or(false, |pending| now_ms >= pending.deadline_ms)
        {
            self.pending = None;
            return Ok(true);
        }
        Ok(false)
    }

    /// Emit and bind one request atomically. Other beacon transmission is blocked
    /// until its response commits, its deadline expires, or trusted code rejects
    /// the request. Retransmission uses a new begin_control call after expiry,
    /// therefore a fresh counter. This does not schedule PCM or choose an RTO.
    pub fn begin_control(
        &mut self,
        request: ControlRequest,
        now_ms: u64,
    ) -> Result<Beacon, SecurityError> {
        self.expire_control(now_ms)?;
        if request.deadline_ms <= now_ms {
            return Err(SecurityError::Malformed);
        }
        let beacon = self.beacon(request.current_mcs, request.target_mcs, request.tx_power)?;
        self.pending = Some(PendingControl {
            counter: self.next - 1,
            expected_ctrl: 0x80
                | (request.target_mcs << 4)
                | ((request.yield_turn as u8) << 3)
                | request.command as u8,
            deadline_ms: request.deadline_ms,
            semantic_ack_base: None,
        });
        Ok(beacon)
    }

    /// Trusted local cancellation, never a reaction to unauthenticated input.
    pub fn reject_control(&mut self, now_ms: u64) -> Result<(), SecurityError> {
        self.expire_control(now_ms)?;
        self.pending = None;
        Ok(())
    }

    pub fn beacon(
        &mut self,
        current_mcs: u8,
        requested_mcs: u8,
        tx_power: u8,
    ) -> Result<Beacon, SecurityError> {
        if self
            .pending
            .map_or(false, |pending| pending.semantic_ack_base.is_none())
        {
            return Err(SecurityError::OutstandingRequest);
        }
        if self.next >= REKEY_COUNTER {
            return Err(SecurityError::RekeyRequired);
        }
        let mut beacon = Beacon {
            current_mcs,
            requested_mcs,
            tx_power,
            sequence: self.next as u8,
            mac: 0,
        };
        beacon.validate()?;
        // A new successful beacon ends the previous duplicate-response context.
        self.pending = None;
        beacon.mac = self.keys.beacon_mac(self.next, &beacon);
        self.next += 1;
        Ok(beacon)
    }
}

/// Ten-failure burst capacity, replenished at one failure per 100 ms.
/// Successful verification never spends credit. Keep across bootstrap resets.
pub(crate) struct MacFailureBudget {
    credit_ms: u64,
    last_time_ms: u64,
    failures: u32,
}

impl MacFailureBudget {
    pub(crate) fn new(now_ms: u64) -> Self {
        Self {
            credit_ms: 1000,
            last_time_ms: now_ms,
            failures: 0,
        }
    }

    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    pub(crate) fn admit(&mut self, now_ms: u64) -> Result<(), SecurityError> {
        self.credit_ms = self
            .credit_ms
            .saturating_add(now_ms.saturating_sub(self.last_time_ms))
            .min(1000);
        self.last_time_ms = self.last_time_ms.max(now_ms);
        if self.credit_ms < 100 {
            Err(SecurityError::RateLimited)
        } else {
            Ok(())
        }
    }

    pub(crate) fn failed_mac(&mut self) -> SecurityError {
        self.credit_ms -= 100;
        self.failures = self.failures.saturating_add(1);
        SecurityError::BadMac
    }

    pub(crate) fn record<T>(
        &mut self,
        result: Result<T, SecurityError>,
    ) -> Result<T, SecurityError> {
        if matches!(result, Err(SecurityError::BadMac)) {
            return Err(self.failed_mac());
        }
        result
    }
}

/// Trusted local measurements/ACK state for an incoming upshift request.
#[derive(Debug, Clone, Copy)]
pub struct McsReplyParameters {
    pub channel_metric: f32,
    pub ack_base: u8,
    pub ack_map: u8,
    pub yield_turn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McsReplyError {
    InvalidLocalParameters,
    NotUpshift,
    InsufficientMetric,
    StaleRequest,
    Security(SecurityError),
}

/// Signed reply and proposed receive boundary, not permission to change PHY.
/// The receiver must await a later authenticated CUR_MCS == target beacon and
/// the sequence boundary before switching. Keep the wire for a scheduled resend;
/// feeding the same request through the verifier again is a replay.
#[derive(Debug)]
pub struct McsCommitReply {
    wire: [u8; 16],
    request_counter: u16,
    target_mcs: u8,
    first_sequence: u8,
}
impl McsCommitReply {
    pub fn codeword(&self) -> [u8; 16] {
        self.wire
    }
    pub fn request_counter(&self) -> u16 {
        self.request_counter
    }
    pub fn target_mcs(&self) -> u8 {
        self.target_mcs
    }
    pub fn first_sequence(&self) -> u8 {
        self.first_sequence
    }
}

/// Per-direction receive replay state and shared PLCP/CCF failure budget.
/// Construct only with keys from a completed fresh bootstrap. Supply trusted
/// monotonic milliseconds; call invalidate() on an extended media outage.
pub struct ControlRx {
    keys: SessionKeys,
    highest: u16,
    bitmap: u64,
    valid: bool,
    last_ccf: Option<u16>,
    failure_budget: MacFailureBudget,
}

impl ControlRx {
    pub fn new(keys: SessionKeys, now_ms: u64) -> Self {
        Self {
            keys,
            highest: 0,
            bitmap: 0,
            valid: true,
            last_ccf: None,
            failure_budget: MacFailureBudget::new(now_ms),
        }
    }

    pub fn mac_failures(&self) -> u32 {
        self.failure_budget.failures()
    }

    /// Trusted outage detection, not an action driven by unauthenticated input.
    /// Further PLCP and CCF input requires a fresh bootstrap/new ControlRx.
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.bitmap = 0;
    }

    fn admit_attempt(&mut self, now_ms: u64) -> Result<(), SecurityError> {
        if !self.valid {
            return Err(SecurityError::ResyncRequired);
        }
        self.failure_budget.admit(now_ms)
    }

    fn failed_mac(&mut self) -> SecurityError {
        self.failure_budget.failed_mac()
    }

    pub fn verify_beacon(&mut self, beacon: Beacon, now_ms: u64) -> Result<u16, SecurityError> {
        if !self.valid {
            return Err(SecurityError::ResyncRequired);
        }
        beacon.validate()?;
        let delta = beacon.sequence.wrapping_sub(self.highest as u8);
        // Exactly half a wire cycle is ambiguous. Do not mutate session state
        // based on this unverified input; trusted outage detection invalidates it.
        if delta == 128 {
            return Err(SecurityError::CounterInference);
        }
        let delta = if delta > 128 {
            delta as i32 - 256
        } else {
            delta as i32
        };
        let candidate = self.highest as i32 + delta;
        // Reject underflow/overflow instead of aliasing a clamped boundary value.
        if candidate < 0 || candidate >= REKEY_COUNTER as i32 {
            return Err(SecurityError::CounterInference);
        }
        let candidate = candidate as u16;
        if candidate <= self.highest {
            let age = self.highest - candidate;
            if age >= 64 || self.bitmap & (1u64 << age) != 0 {
                return Err(SecurityError::Replay);
            }
        }
        self.admit_attempt(now_ms)?;
        let tag = self.keys.beacon_mac(candidate, &beacon);
        if !bool::from(tag.ct_eq(&beacon.mac)) {
            return Err(self.failed_mac());
        }
        if candidate > self.highest {
            let shift = candidate - self.highest;
            self.bitmap = if shift >= 64 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest = candidate;
        } else {
            self.bitmap |= 1u64 << (self.highest - candidate);
        }
        Ok(candidate)
    }

    pub(crate) fn is_latest_beacon(&self, counter: u16) -> bool {
        self.valid && self.bitmap != 0 && counter == self.highest
    }

    /// Engine-only ACK signing for its latest admitted peer beacon. The engine
    /// retains this counter until that burst's payload has been processed.
    pub(crate) fn sign_latest_ack(&self, counter: u16, mcs: u8, base: u8, map: u8)
        -> Result<[u8;16], SecurityError> {
        if !self.valid { return Err(SecurityError::ResyncRequired); }
        if self.bitmap == 0 || counter != self.highest { return Err(SecurityError::Replay); }
        if mcs > 4 || map > 127 { return Err(SecurityError::Malformed); }
        self.keys.sign_ccf(counter, CompactControlFrame {
            ccf_ctrl: 0x89 | (mcs << 4), ack_base: base, ack_map: map, ccf_mac: 0,
        })
    }

    /// Live scheduler has already authenticated this latest request and owns
    /// its retained boundary. Do not re-admit the beacon through replay state.
    pub(crate) fn sign_latest_mcs_commit(&self, counter: u16, mcs: u8, base: u8)
        -> Result<[u8; 16], SecurityError> {
        if !self.valid { return Err(SecurityError::ResyncRequired); }
        if self.bitmap == 0 || counter != self.highest { return Err(SecurityError::Replay); }
        if mcs > 4 { return Err(SecurityError::Malformed); }
        self.keys.sign_ccf(counter, CompactControlFrame {
            ccf_ctrl: 0x88 | (mcs << 4) | ControlCommand::McsCommitAck as u8,
            ack_base: base, ack_map: 0, ccf_mac: 0,
        })
    }

    /// Authenticate a fresh peer upshift request, check trusted local metric
    /// M >= 0.85, and sign its response using the inferred full counter.
    /// Call this instead of verify_beacon for this request, using the same Rx
    /// owner as ordinary traffic. Authenticated refusals consume replay state.
    /// No ownership, payload-rate or receive-sequence state is changed here.
    pub fn prepare_mcs_commit(
        &mut self,
        beacon: Beacon,
        local: McsReplyParameters,
        now_ms: u64,
    ) -> Result<McsCommitReply, McsReplyError> {
        if !local.channel_metric.is_finite()
            || !(0.0..=1.0).contains(&local.channel_metric)
            || local.ack_map & 0x80 != 0
        {
            return Err(McsReplyError::InvalidLocalParameters);
        }
        let counter = self
            .verify_beacon(beacon, now_ms)
            .map_err(McsReplyError::Security)?;
        // Replay windows admit delayed ordinary traffic; control policy must
        // not revive an older request after a newer verified beacon.
        if counter < self.highest {
            return Err(McsReplyError::StaleRequest);
        }
        if beacon.requested_mcs <= beacon.current_mcs {
            return Err(McsReplyError::NotUpshift);
        }
        if local.channel_metric < 0.85 {
            return Err(McsReplyError::InsufficientMetric);
        }
        let wire = self
            .keys
            .sign_ccf(
                counter,
                CompactControlFrame {
                    ccf_ctrl: 0x80
                        | (beacon.requested_mcs << 4)
                        | ((local.yield_turn as u8) << 3)
                        | ControlCommand::McsCommitAck as u8,
                    ack_base: local.ack_base,
                    ack_map: local.ack_map,
                    ccf_mac: 0,
                },
            )
            .map_err(McsReplyError::Security)?;
        Ok(McsCommitReply {
            wire,
            request_counter: counter,
            target_mcs: beacon.requested_mcs,
            first_sequence: local.ack_base.wrapping_add(1),
        })
    }

    /// Verify against the counter recorded by begin_control, not a caller or
    /// wire-supplied inferred counter. Valid duplicate replies can refresh ACKs
    /// until the deadline or next beacon, but never repeat control side effects.
    /// The tx/rx owners must belong to the same session and local endpoint.
    pub fn verify_control_response(
        &mut self,
        tx: &mut ControlTx,
        wire: [u8; 16],
        erasures: &[usize],
        now_ms: u64,
    ) -> Result<ControlResponse, SecurityError> {
        if !self.valid {
            return Err(SecurityError::ResyncRequired);
        }
        tx.expire_control(now_ms)?;
        let pending = tx.pending.ok_or(SecurityError::NoPendingRequest)?;
        let frame = CompactControlFrame::decode(wire, erasures)
            .map_err(|_| SecurityError::ChannelIntegrity)?;
        let command = frame.ccf_ctrl & 7;
        if frame.ccf_ctrl & 0x80 == 0
            || (frame.ccf_ctrl >> 4) & 7 > 4
            || !matches!(command, 1..=3)
            || frame.ack_map & 0x80 != 0
        {
            return Err(SecurityError::Malformed);
        }
        let duplicate = pending.semantic_ack_base.is_some();
        if self.last_ccf.map_or(false, |last| {
            last > pending.counter || (last == pending.counter && !duplicate)
        }) {
            return Err(SecurityError::Replay);
        }
        self.admit_attempt(now_ms)?;
        if !bool::from(
            self.keys
                .ccf_mac(pending.counter, &frame)
                .ct_eq(&frame.ccf_mac),
        ) {
            return Err(self.failed_mac());
        }
        if frame.ccf_ctrl != pending.expected_ctrl {
            return Err(SecurityError::UnexpectedControl);
        }
        let boundary = pending.semantic_ack_base.unwrap_or(frame.ack_base);
        tx.pending
            .as_mut()
            .expect("pending context retained")
            .semantic_ack_base = Some(boundary);
        self.last_ccf = Some(pending.counter);
        Ok(ControlResponse {
            frame,
            apply_semantics: !duplicate,
            commit_sequence: if command == ControlCommand::McsCommitAck as u8 {
                Some(boundary.wrapping_add(1))
            } else {
                None
            },
        })
    }

    /// `expected_counter` must be the locally recorded transmit counter of the
    /// burst being acknowledged, never a counter supplied by the peer.
    /// Successful CCFs are accepted only once per counter.
    pub fn verify_ccf(
        &mut self,
        wire: [u8; 16],
        erasures: &[usize],
        expected_counter: u16,
        now_ms: u64,
    ) -> Result<CompactControlFrame, SecurityError> {
        if !self.valid {
            return Err(SecurityError::ResyncRequired);
        }
        let frame = CompactControlFrame::decode(wire, erasures)
            .map_err(|_| SecurityError::ChannelIntegrity)?;
        if expected_counter >= REKEY_COUNTER {
            return Err(SecurityError::RekeyRequired);
        }
        if self.last_ccf.map_or(false, |last| expected_counter <= last) {
            return Err(SecurityError::Replay);
        }
        self.admit_attempt(now_ms)?;
        if !bool::from(
            self.keys
                .ccf_mac(expected_counter, &frame)
                .ct_eq(&frame.ccf_mac),
        ) {
            return Err(self.failed_mac());
        }
        self.last_ccf = Some(expected_counter);
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_reference_vectors() {
        let key = core::array::from_fn(|i| i as u8);
        assert_eq!(siphash(&key, &[]), 0x726fdb47dd0e0e31);
        let message: [u8; 15] = core::array::from_fn(|i| i as u8);
        assert_eq!(siphash(&key, &message), 0xa129ca6149be45e5);
    }

    #[test]
    fn authenticated_wrong_epoch_does_not_finish_transaction() {
        let mut pending = PendingBootstrap::new([1; 16], [2; 16]);
        let good = SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]);
        let wire = bootstrap_wire(
            ACCEPT,
            &[3; 16],
            good.epoch.wrapping_add(1),
            &[1; 16],
            Some(&[2; 16]),
        );
        assert!(matches!(
            pending.finish(&wire),
            Err(SecurityError::EpochMismatch)
        ));
        assert!(pending.request().is_ok());
    }
}
