# Host-owned PCM handshake coordinator

`handshake::HandshakeCoordinator` connects the opt-in Barker bootstrap profile,
host session manager, and authenticated PLCP confirmation receiver. It supports
8 kHz signed 16-bit mono PCM and MCS 2/3 confirmation. It is a Rust worker-thread
component, not an audio callback or a new C ABI. It constructs waveforms and may
allocate or request entropy. Real-time adapters must move PCM through bounded
queues and run these methods on a host worker. The Rust
[HandshakeBridge](HANDSHAKE_BRIDGE.md) now provides these queues, routing and
device-drain fences; actual device adapters must supply drain completion.

## Sequence and ownership

1. Construct one coordinator per peer using the shared PSK, desired MCS, and a
   trusted monotonic millisecond clock. Production calls use `OsNonceSource`.
   An endpoint may remain listening or call `begin`. Both may initiate: existing
   nonce ordering chooses their roles independently of phone/PBX device types.
2. Feed captured samples to `process_pcm`, request playback via `render_pcm`, and
   call `poll` during capture/playback gaps. The coordinator acquires request and
   accept frames, authenticates them, and schedules bootstrap retries. One pending
   transmission is retained; duplicate requests never restart an active waveform.
3. An authenticated accept schedules an initiator confirmation: a real MAC-bearing
   PLCP header plus an empty best-effort canonical frame with yield. It consumes
   the initiator's counter 0. The initiator becomes `Ready` after the entire
   confirmation waveform has been rendered. No application data passes through
   the coordinator.
4. The provisional responder verifies incoming PLCP under the derived keys. It
   becomes `Ready` only after a valid MCS 2/3 beacon and completion of any active
   accept playback. The replay bitmap already includes that confirmed counter.
5. Drain queued handshake playback to the audio device before changing routing.
   `Ready` means rendered, not physically played. Call `take_established` once,
   then `HostHandle::install_session`. On engine queue saturation, retain the
   returned transfer and retry installation. Queue application packets only
   after successful installation; observe `authenticated_ready` on the engine.
6. Route subsequent PCM to the engine. It receives the existing counters and
   replay state without reconstruction. The coordinator enters `Transferred`
   and retains nonce history and its bootstrap MAC-failure budget.

Only one thread owns a coordinator. `process_pcm` chunks large input internally
into at most 160 samples before using the bounded PHY receiver. PCM and trusted
clock time must remain correctly paced; supplying seconds of audio at one
instant is not a real-time deployment. Render fills unused output with silence
and starts at most one waveform per call. Pending jobs start at the next call,
so there can be up to one callback of additional silence between transmissions.

`reset` stops local handshake media and retires the session while retaining
nonce history and failure credits. An adapter must separately clear its old
PCM queues and reset/close any installed engine. Starting a new handshake after
transfer requires this explicit reset. `TimedOut` likewise requires a new begin
or reset; it does not restart an unlimited background negotiation loop.

## Error and loss behavior

CRC errors, malformed frames, failed tags, replays, and rate-limited bootstrap
attempts produce no response. Cumulative rejected bootstrap frames and actual
bootstrap MAC failures have separate getters. Entropy, nonce-collision, cache,
and trusted-clock errors return to the host. On an error during capture, the
remaining input may be discarded; the host must not replay the same PCM chunk.
The provisional PLCP verifier retains its failure budget in the session transfer;
coordinator-level PLCP failure events are not yet exposed to the host.

A lost request or accept is recovered by the session manager's bounded retries.
If the initial PLCP confirmation is lost, the initiator may already have
transferred to its engine while the responder remains provisional. A subsequent
valid data beacon can confirm the responder. The coordinator discards that
burst's payload; reliable engine retransmission with a fresh counter recovers
it after the responder handoff. Best-effort data in that burst can be lost.
Coordinator-created initiator transfers now arm three additional authenticated
confirmation opportunities in the engine, at least six seconds apart on its
rendered-sample clock. With no outgoing data, each sends one empty best-effort
canonical frame under a fresh beacon counter. Existing outgoing data can serve
as a due probe. A verified peer beacon cancels the remaining probes; forged,
replayed or physically invalid headers do not. Responder transfers do not arm
probes, and empty feedback never requests an ACK, so this cannot create an
endless idle exchange. Reset or counter exhaustion closes the recovery path.

This is an explicit project recovery profile, separate from the spec's bootstrap
request RTO schedule. It is not a mutual readiness acknowledgment. Retries respect
active bursts and existing peer-turn waits; correctly paced rendering and prompt
handoff are required. If all probes are lost or the responder expires before
recovery, a fresh handshake is still required. Direct SessionManager transfers
retain their previous behavior and do not automatically arm these probes.

The coordinator does not supply a full duplex/TDD scheduler, acoustic collision
avoidance, transport-preserving rekey, automatic outage detection, CCF transactions,
or a C entry point. Bootstrap and PLCP retain their documented project-profile
wire conventions. Codec, clock-drift, cellular, and hardware performance remain
unqualified. PLCP integrity does not authenticate canonical data/ACK contents;
SSH/Mosh remain responsible for application security.

## Verification

`tests/handshake_coordinator.rs` exercises end-to-end request/accept/confirmation
PCM followed by authenticated engine packets in both directions for MCS 2/3,
80- and 511-sample chunks, a lost accept, simultaneous initiation, forged PSK and
PLCP input, delayed confirmation handoff, lost confirmation recovered through
reliable data retransmission, provisional expiry, reset, entropy failure,
nonce-cache preservation, unsupported profiles and backward clocks.
