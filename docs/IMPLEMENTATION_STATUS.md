# Implementation status and continuation plan

Reviewed 2026-09-26 against SPEC.md v3.8.10, the working tree, PROJECT.md,
ORIGINAL_REQUEST.md, and the previous agents' handoffs. This is a research
prototype, not a deployable SSH/Mosh platform. Software loopback success does
not establish vocoder, cellular-call, or hardware performance.

## Active handoff — 2026-09-26

Current checkpoint: live negotiated MCS2→3 and explicit emergency return to MCS2
are implemented in the Rust Endpoint. Emergency downshift requires trusted local
M < 0.60, preserves unacknowledged data and cooldown, and cancels pending upshifts.
It applies after buffered audio finishes and announces the lower rate when idle.
The sender drains already-admitted ARQ work, retains host-queued packets, sends
old-rate requests and applies the verified commit only at the next REL_SEQ
boundary. The receiver retains its plan across retries and requires a verified
target PLCP plus a matching forward sequence before admitting target-rate data.

Public host APIs: request_upshift(3), emergency_downshift(M), set_channel_metric(M),
rate_change_status().
Metrics are explicit trusted host input, absent by default and cleared on reset;
no automatic estimator is claimed. Full-duplex directions can use different rates.
Best-effort data stays old-rate until reliable traffic reaches the commit boundary.
Three fresh-counter attempts precede a ten-second cooldown. Lost/forged replies,
unsupported target rates, queue pressure and reset do not bypass the guard.

Verification passed: 318 tests (311 runtime and seven compile-fail doctests),
including all 19 Endpoint tests and the buffered-burst PHY regression. Callback
allocation/deallocation checks pass through handoff, negotiation, data and emergency
downshift. All-target compilation is warning-free; git diff --check passes.
No work or verification is running. Final logs: `/tmp/vradm-downshift-final.log`
and `/tmp/vradm-downshift-check.log` (temporary). Previous checkpoint: 315 tests.
See [LIVE_MCS.md](LIVE_MCS.md) for the conservative drained-window restrictions,
API semantics and proof scope. SPEC.md is unchanged.

Remaining integrated work: general mode transitions and automatic metrics,
acoustic TDD ownership, authenticated C lifecycle, mutual readiness and automatic
outage/rekey coordination. Missing PHY modes, timing recovery, SOTP and qualification
still block M1. Next focused integration slice: expose this tested full-duplex
lifecycle through C with explicit host/audio ownership. Acoustic TDD remains a
separate scheduling requirement. See [M1_PLAN.md](M1_PLAN.md). Legacy C construction
is unauthenticated.

Changes: established session counters move exactly once via `SessionTransfer`
into the host command queue. The authenticated boxed Rust engine gates PCM until
installation at a burst boundary, transmits real beacon MACs, verifies before
payload/MCS/ARQ admission, preserves replay history, uses role-based dither DIR,
and reports actual MAC failures through tamper telemetry. Reset/counter exhaustion
close the path without unauthenticated fallback. Installation starts a new local
packet generation and drops old queued/ARQ state. Details and limitations are in
`docs/AUTHENTICATED_ENGINE.md`.

Coverage: MCS 2/3 bidirectional traffic, authenticated loss recovery, forged/replayed
headers, replay preservation through handoff, complete-guard verification timing,
physical erasure separation, queue-full retry, key replacement at burst boundaries,
reset/rekey lockout, unsupported profiles, non-cloneable transfers, and zero
callback allocations/frees. Final log: `/tmp/vradm-auth-final.log` (temporary).

Next core work: general MCS transitions and acoustic control-turn scheduling;
mutual readiness acknowledgment; trusted outage/rekey timers; host events and
C ABI selection. Platform audio adapters and actual device-drain callbacks still need
implementation/qualification. Cache persistence
remains open. Canonical payloads/ACK fields are not MAC-protected by PLCP; application authentication remains at SSH/Mosh.
No codec/hardware/drift qualification or complete M1 acceptance is claimed.

Preserve existing uncommitted work; no commits have been made. This document is
the model handoff. Earlier checkpoint before limiter correction: 302 tests.

## Live emergency downshift

`EndpointHost::emergency_downshift(M)` queues a return to MCS2 for explicit trusted
0 <= M < 0.60. It applies at a burst boundary, cancels this direction's outstanding
control/upshift, preserves ARQ data and cooldown, and announces the lower rate even
when idle. Buffered/device-queued audio cannot be retracted. A pending emergency
blocks a new upshift; queue-full failure leaves admission intact. The independent
receive metric and reverse-direction plan are unchanged. See LIVE_MCS.md.

Three new endpoint tests cover idle announcement, validation/queue pressure,
finishing buffered audio, lower-rate retry of lost data/ACK, cancellation of pending
and accepted upshifts, command ordering and later renegotiation. Existing tests
now verify cooldown preservation and zero callback allocations through downshift.
Automatic metric estimation/adaptation and other MCS modes remain unimplemented.

## Live negotiated MCS2→3

The endpoint now owns sender drain/retry/commit state and receiver plans alongside
its existing ControlTx/ControlRx. No external complete_commit assertion or signed
reply injection is needed for successful operation. Six new endpoint tests cover
success, loss/forgery, metric refusal, abort/cooldown, BE before REL, active-window
drain across 255→0, reset of an accepted plan/metric, queue pressure, and rejection
of authenticated target-rate traffic without a plan. A PHY regression test proves
buffered bursts preserve their own verified-beacon context. Allocation coverage
now includes negotiation. See LIVE_MCS.md and SPEC_CONFORMANCE.md.

## Nominal limiter correction

The data-burst conditioner now applies tanh only above 0.45 FS; nominal samples
remain linear through quantization. f64 energy/gain avoids overflow from finite
f32 inputs and accidental threshold crossings due to gain rounding. Non-finite
input/invalid target silences only the addressed output. Direct backstop tests
cover exceptional peaks; carrier-projection tests require conditioner-only error
below 0.1%, with per-sample error at PCM quantization scale. See TX_AMPLITUDE.md.

The literal exceptional formula still has a discontinuous join to the linear
region; this is documented for spec clarification and is not reached by nominal
peak normalization. Full TC-10c and codec/CPU qualification remain incomplete.

## Live CCF feedback integration

Endpoint now opens counter-bound standalone ACK transactions for reliable bursts,
signs replies from verified peer burst context, renders CCF audio and acquires
incoming replies with a ten-phase streaming search. Verified replies update live
ARQ and close the transaction. Loss/forgery causes a fresh-counter retry with no
duplicate delivery. Canonical ACK-only bursts are disabled for this opt-in profile;
legacy constructors retain their behavior. Raw rate commands are rejected here; the typed live negotiation path is separate.

[LIVE_CCF.md](LIVE_CCF.md) records the explicit full-duplex assumptions, fixed
12-second sample-clock timeout, detectable-sync requirement and unqualified CPU/
codec behavior. This is not acoustic TDD ownership or exact EOT timing. The live
MCS2→3 path above now applies negotiated boundaries within this same engine.

## Integrated endpoint lifecycle

`Endpoint` now binds coordinator, bridge and authenticated engine behind unique
host/audio handles. Host pumping performs automatic drain-gated session transfer;
packet admission waits for audio-side installation. Reset closes admission and
resets both layers while preserving nonce history. Buffered old PCM is drained
before handshake routing, avoiding a stranded-ring reset deadlock. Callers cannot
mix another engine into the bridge or bypass coordinated reset via raw commands.

[ENDPOINT.md](ENDPOINT.md) describes the 8 kHz MCS2/3 profile, one-time
split, device-drain contract, graceful reset and limits. Public-API tests cover
bootstrap, bidirectional packets, loss recovery and rekey. This advances the first
M1 integrated checkpoint; standalone CCF feedback is now wired into this endpoint,
and the drained-window MCS2→3 path is integrated. General transitions and acoustic
TDD remain open. No acoustic/device qualification or
complete M1 is claimed.

## Live transmit amplitude integration

Engine configuration now reaches PHY RMS conditioning. SET_TX_PARAMS applies
param_f32 as a validated RMS ceiling between buffered bursts; invalid commands
are rejected before queueing. C/authenticated constructors reject NaN/infinity.
Applied settings survive link resets; zero mutes but does not pause protocol
state. [TX_AMPLITUDE.md](TX_AMPLITUDE.md) documents limits and tests.

[M1_PLAN.md](M1_PLAN.md) now separates the next integrated endpoint checkpoint,
remaining full-core work and later platform milestones. Prioritize live endpoint
integration over additional standalone control helpers. Test count is not a
milestone completion metric.

## Receiver MCS commit response follow-up

`ControlRx::prepare_mcs_commit` authenticates a fresh upshift request using the
existing receive replay state/budget and requires trusted channel metric >= 0.85.
It signs the CCF with the full inferred request counter and returns the target
and ACK_BASE+1 boundary. Invalid local parameters do not consume the request;
authenticated refusals do. Delayed requests cannot revive older negotiations.
An audio roundtrip test connects this reply through CCF turn PCM to the sender's
deferred commit policy. See [MCS_CONTROL.md](MCS_CONTROL.md).

No live response scheduling or receiver-rate switchover is enabled. The future
scheduler must preserve the selected boundary across retries, discard stale
replies and wait for the authenticated target-rate PLCP before switching.
Metric measurement, acquisition and actual payload-sequence application remain
open; the API consumes a trusted local metric rather than inventing one.

## Aligned CCF receive turn follow-up

`CcfTurnReceiver` now admits the aligned CCF followed by all fifteen 10 ms
EOT windows and the full 150 ms guard. Both 1400/1800 Hz tones must pass bounded
energy/ratio tests. It reports exact chunk consumption and does not return even
a channel-valid frame before guard completion. Bad CCF/EOT also consume the
whole turn. Reset discards partial evidence; capture gaps require a fresh aligned
boundary. Success returns UnverifiedCcf for the existing authenticated guard.

[CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md) records thresholds and limitations.
The detector is a conservative aligned software profile, not unknown-boundary
acquisition, codec qualification or authorization to change channel ownership.
MAC/transaction checks and actual device-drain integration remain mandatory.

## CCF transmit turn follow-up

`ccf_turn::CcfTurnTransmitter` renders the aligned CCF plus a 150 ms dual-tone
EOT and 150 ms silent guard. It accepts only exact, structurally valid yielding
codewords, rejects replacement throughout the turn and supports explicit local
cancellation/reuse. The EOT profile interprets -12 dBFS as composite RMS; its
1400/1800 Hz components and peak limit are covered by tests. Callback operations
use fixed memory. See [CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md).

This is a transmit component, not the live TDD scheduler. Trusted callers still
supply signed frames and transmission permission. Render completion cannot prove
device drain or authorize ownership transfer; queued audio requires separate
invalidation on cancel. Unknown-boundary receive acquisition, collision recovery,
playback acknowledgments and live engine integration remain open.

## Aligned CCF PCM follow-up

`ccf_phy` supplies a separate, opt-in 8 kHz single-tone pitch transmitter and
aligned NCCF receiver for 32-symbol CCFs. It discards the first 15 ms per symbol,
deduplicates nibble erasures into RS byte erasures and consumes the complete
1.6-second frame even on failure. Channel admission returns UnverifiedCcf;
existing control/MCS owners still perform MAC and transaction verification.
Rendering and decoding use fixed memory. Tables initialize off the audio thread.

[CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md) specifies phase, taper, nibble order,
amplitude and exact-boundary assumptions. Targeted loopback, erasure, forgery,
MCS handoff and allocation tests pass. Acquisition, clock tracking, live TDD/ARQ
integration and codec/device qualification remain absent. This does not change
the live engine's MCS0 data fallback.

## MCS negotiation policy follow-up

`McsNegotiator` exclusively borrows the local control owners. It emits at most
three upshift attempts with fresh counters, then aborts to the current requested
rate and enforces ten seconds of cooldown. Data beacons remain available during
cooldown. Delayed polling emits one retry, and clock overflow cannot bypass the
cooldown. Verified raw CCF responses produce a saved sequence-boundary commit plan;
current rate stays old until trusted scheduler completion. Duplicate ACK updates
cannot move that plan. Local cancel/emergency downshift preserves prior cooldown.

[MCS_CONTROL.md](MCS_CONTROL.md) documents the API and remaining media work. No
live modulation/receiver switchover is enabled; metric estimation, CCF
acquisition/live scheduling, dynamic RTO and sequence-aware payload application remain
missing. Keep the policy alive across attempts to retain its cooldown. Full-suite
log: `/tmp/vradm-mcs-control-final.log` (temporary).

## Authenticated CCF transaction follow-up

`ControlTx::begin_control` records one local full-counter request and blocks
intervening beacons while awaiting a matching response. Trusted expiry/rejection
releases it; a retried request uses a fresh counter. `ControlRx` verifies CCFs
against that owner, checks expected command/target/yield semantics and uses the
same PLCP/CCF MAC-failure budget. Authenticated duplicate responses can refresh
ACK fields but cannot repeat control effects or move the first MCS commit boundary.
Duplicates expire at the original deadline or next successful outgoing beacon.

[CONTROL_TRANSACTIONS.md](CONTROL_TRANSACTIONS.md) covers API ownership, the
unchanged strict low-level verifier, wire-wrap/MAC8 limits, and validation. This
does not add an automatic MCS switch, TDD grant, or C API. Aligned CCF PCM now
exists separately; acquisition, media scheduling and live ARQ/control dispatch remain missing. The
MCS policy now supplies three-attempt retry/cooldown and deferred commit plans. Full log: `/tmp/vradm-control-final.log` (temporary).

## Idle confirmation recovery follow-up

Only initiator transfers produced by HandshakeCoordinator arm three confirmation
probes in the engine. Rendered 8 kHz samples pace them at least six seconds apart;
active bursts and peer-turn waits are respected. A due transmission carries data
already available, otherwise an empty best-effort frame. Every retry consumes a
fresh beacon counter; no old MAC or replay history is reused. Any verified peer
beacon cancels remaining probes. Forgery/replay/physical erasure cannot cancel
them. Reset and counter exhaustion close the path as before. Responder and direct
SessionManager transfers retain their previous behavior without armed probes.

Targeted tests recover an idle provisional responder after losing the original
confirmation and first two engine probes, for MCS 2/3. They verify counters 1/2/3,
no early/continuous retry traffic, cancellation by verified peer but not forgery,
reset cancellation, and zero allocations/frees. This is an explicit project
profile, not the bootstrap request RTO or a mutual readiness transaction. If all
probes are lost, rendering stalls or handoff arrives after responder expiry,
fresh bootstrap remains necessary. Full-suite log:
`/tmp/vradm-confirmation-final.log` (temporary).

## Handshake PCM bridge follow-up

`HandshakeBridge` provides two bounded SPSC PCM queues with unique host/audio
owners. `HandshakeWorker` pumps the host coordinator, retains rendered PCM under
backpressure, seals playback with a fence, and transfers counters only after a
device-playback acknowledgment. Opaque fence tokens bind acknowledgments to one
bridge generation, rejecting late or wrong-bridge completions. Queue-full engine
admission retains the transfer; routing changes only after successful admission.

Audio wrappers copy handshake PCM or route to the authenticated engine, while
servicing bounded engine command batches without emitting data audio early.
Capture gaps clear partial physical acquisition. Reset generations keep stale
queued/partial PCM and acknowledgments out of later handshakes. Tests cover
concurrency, saturation, gaps, timeout/reset, drain tokens, installation retry,
zero allocations/frees, and bridged MCS 2/3 bidirectional data. Full-suite log:
`/tmp/vradm-bridge-final.log` (temporary).

[HANDSHAKE_BRIDGE.md](HANDSHAKE_BRIDGE.md) documents the supported adapter and the
explicit platform obligations: actual device buffers must drain before token
acknowledgment; bridge reset cannot retract device audio or reset an installed
engine. Bounded idle confirmation recovery is now implemented; mutual readiness
and automatic outage/rekey remain unfinished.

## PCM handshake coordinator follow-up

`handshake::HandshakeCoordinator` now runs the Barker request/accept acquisition,
automatic retries, nonce-based simultaneous-initiation resolution, and provisional
PLCP confirmation on a host worker. Initiators render a signed empty confirmation
before handoff; responders must authenticate the peer beacon. Existing counters
and replay windows then transfer once into the data engine. Nine targeted tests
pass, including bidirectional authenticated engine traffic after the full PCM
handshake, lost accept recovery and lost confirmation recovery via reliable data.
Full-suite log: `/tmp/vradm-handshake-final.log` (temporary).

See [HANDSHAKE_COORDINATOR.md](HANDSHAKE_COORDINATOR.md) for the actual supported
path and adapter contract. This is not a real-time callback implementation:
waveform generation/entropy remain on the worker, playback must be drained before
switching routes. The bounded PCM bridge now supplies queues/routing and drain
fences; real device adapters must provide truthful completion acknowledgments.
The first data burst used as confirmation is discarded; reliable retransmission
recovers it, but best-effort data can be lost. Initiator handoffs now arm three
idle confirmation probes in the engine. Fresh bootstrap remains necessary when
all probes fail or the responder expires before recovery. Mutual readiness
exchange and seamless rekey remain unimplemented.

## Bootstrap failure-budget follow-up

`SessionManager::receive` now gates bootstrap SipHash verification with a fixed
budget shared by request and accept frames: ten initial failure credits, refilling
one per 100 trusted milliseconds, capped at ten. This uses the same budget
implementation as `ControlRx`; bootstrap and established PLCP/CCF keep separate
budget instances because they have separate host/audio owners. This is not a
process-wide combined limit or a strict sliding one-second maximum.

CRC/format failures do not spend credit. Valid MACs do not spend credit either;
once depleted, even a valid frame waits for refill because checking it would
require another hash. A rate-limited input produces no reply or entropy request,
and does not alter session state except ordinary trusted-time expiry. Scheduled
local retransmissions continue. `reset`, transaction completion and session
handoff retain the manager's budget and cumulative `bootstrap_mac_failures()`;
recreating the manager resets both. Direct low-level bootstrap primitives remain
unthrottled and must be coordinated by their caller.

Four new integration tests cover request/accept budget sharing, no entropy/state
consumption on failed admission, timed recovery, valid duplicate retries, CRC
and malformed input, reset persistence, backward clock rejection and capped refill
at `u64::MAX`. Existing PLCP/CCF limiter tests cover the extracted shared helper.
Verification log: `/tmp/vradm-bootstrap-throttle-final.log` (temporary).

## What exists

- `vradm-core`: CRC, canonical/compact frame codecs, Golay and Reed–Solomon,
  interleaving, fragmentation/reassembly, selective acknowledgments, PCM PHY,
  and the C-ABI engine surface.
- MCS 2/3 software PCM loopback and bounded, preallocated audio buffers.
- Unit, C-ABI, allocation, concurrency, PHY, and ARQ integration tests under
  `vradm-core/tests`. The old ignore rule hid this directory from Git; it is
  now explicitly included.
- A root Cargo workspace containing the existing core crate. Planned platform
  crates are not listed as members until they exist.

## Repairs in this continuation

1. A receiver with no outgoing application packet now emits legal §2.1
   zero-payload canonical feedback. These frames use the best-effort class,
   do not consume reliable sequence numbers, do not enter fragment reassembly,
   and never solicit feedback themselves. Data frames still piggyback ACKs.
   The initial cumulative ACK is 255, preceding initial sequence 0.
2. A sender waits for feedback after draining its burst and retransmits after
   a bounded sample-clock timeout, without requiring another host write.
   New host traffic cannot bypass that wait. The deadline allows a maximum
   peer burst plus a one-second margin. This is a conservative prototype
   timer, **not** the spec's adaptive RTT/RTO, Karn, or acoustic collision
   recovery implementation.
3. Data frame CTRL now carries the actual transmitting MCS.
4. Telemetry uses atomic words in both snapshot slots. Ordinary struct reads
   from rotating UnsafeCell slots could race with slot reuse even if the
   sequence check later rejected the snapshot. The writer publishes through
   a sequence counter; readers retry coherent atomic copies. Single-writer
   ownership is checked without waiting; reads are permitted from multiple
   diagnostic threads. This fixes telemetry, not every unsafe Rust API.
5. The unfinished round-3 challenge suite contained two incorrect expectations:
   80 bytes produce three 37-byte fragments, not four; and the eighth forward
   frame cannot be selectively reported in the seven-bit wire ACK bitmap.
   The fixtures now use 37 bytes per fragment and expect that additional
   retransmission until cumulative feedback advances. Delivery assertions
   remain intact. Its third failure was the genuine missing-feedback stall.

## Receive-path hardening follow-up

- Validate payload bounds, IP mode, wire version, MCS, fragment index/count,
  and agreement between frame and fragment classes before receiver state changes.
  Conflicting fragment counts within an active packet are rejected before ACK
  advancement. Initialized reliable receivers reject frames outside their
  eight-frame forward window. New canonical frames default to wire version 3.8;
  older duplicate-frame fixtures were corrected to carry that version explicitly.
- Protect incomplete reliable reassembly contexts from best-effort eviction.
  Best-effort eviction uses its own sequence space to measure age. If all
  contexts hold incomplete reliable packets, new packets are refused without
  acknowledging data that cannot be stored.
- When the host's 64-packet receive queue is full, do not ingest additional data
  fragments or advance their ACK state. Continue processing reverse-link ACKs
  and yield flags so outgoing traffic can progress. Reliable senders retain and
  retry blocked data; best-effort traffic may be dropped under pressure.
  This relies on the existing single audio producer / single host consumer
  ownership contract. Once capacity is available, retransmission delivers data.
- A too-small host polling buffer leaves the queued packet intact for retry.
- GMD confidence ranking now sorts in place with a byte-index tie-breaker.
  The previous stable sort allocated scratch space on the audio thread when
  enough bytes had low confidence. Allocation tests cover both this path and
  repeated retransmission against a full receive queue.

Four new admission tests failed against the preceding implementation: an
oversized length panicked, contradictory fragment counts were accepted,
best-effort pressure evicted an acknowledged reliable fragment, and an
out-of-window frame was delivered. All four now pass. PCM tests also fill the
host queue, block a 256-byte packet, carry reverse traffic, resume host polling,
and verify eventual single delivery with no further retransmissions.

## Ordered reliable delivery follow-up

Complete reliable packets stay in reassembly storage until all earlier packets
have been delivered. A recovered gap can make multiple packets ready; callers
using the low-level API must drain `poll_packet_into` after ingestion. The engine
does this automatically, including on render callbacks with no new incoming PCM.
The receive queue's sole producer checks capacity before consuming held packets.
Acknowledged reliable storage cannot be evicted by best-effort traffic, including
when every context is occupied. New packets are then refused without ACK until
space is reclaimed. No additional heap-backed queue was introduced.

Tests cover eight packets behind one loss, reverse arrival order and wraparound,
best-effort bypass, full context storage, reset, reliable keepalives, and malformed
packet ranges that overlap another outstanding packet or extend a previously
delivered one. The PCM test fills 63 host slots, delivers a later packet before a
missing packet's retransmission, then verifies FIFO delivery and release of the
last held packet when the host resumes reading. An allocation counter verifies
zero heap allocation/deallocation during ingestion and ordered gap release.

## Queued reset and ownership follow-up

Previously a RESET_SESSION command invoked the full, quiescent-only lifecycle
reset from the audio callback. That consumed the host-owned receive queue,
dropped subsequent commands, and freed SOTP Vec buffers on the audio thread.

Queued reset now carries a local generation in an internal command envelope.
Only a successful enqueue publishes the new host generation and clears current
host-owned object staging. Subsequent writes use that generation. The audio
owner applies queued link resets in FIFO order at a transmit-burst boundary,
and skips older TX packets without consuming future-generation packets. The host
skips older RX packets, including ones audio publishes after host cleanup. The
queue cursors are never moved by a second consumer during queued reset.

Generation fields are local implementation details, distinct from the spec's
cryptographic session epoch. There is no remote reset handshake yet; callers must
coordinate the peer. This cannot reject replayed or delayed old wire frames after
a link reset. Direct `vradm_reset`/destroy still require complete quiescence.
SOTP start/stop and background decoding remain unimplemented; future decoder
integration must establish ownership handoff before accessing host staging.

## Rust ownership API follow-up

`SpscQueue` now requires exclusive access for local mutation, or `split()` into
one movable producer and one movable consumer. Shared raw queue operations are
private and unsafe. A peek reference borrows the consumer until it is no longer
used, preventing removal/reuse underneath it. Endpoints are not Sync, including
for Send-but-not-Sync payloads. Queue drop drains retained values; occupancy
observations under concurrent activity are bounded estimates. A unit test now
crosses actual `usize::MAX`, beyond the existing repeated ring-slot reuse test.

`vradm_engine::split()` borrows an engine into non-cloneable `HostHandle` and
`AudioHandle`. Mutating operations require mutable access to their endpoint;
both audio callbacks use the same audio handle. `reset_exclusive()` is available
once the endpoints return. Shared-reference raw engine operations remain available
for FFI/integration, but are explicitly unsafe with documented ownership rules.
C function signatures and public C data layouts are unchanged. Rust queue callers
must migrate from shared Arc mutation to scoped endpoint ownership.

Coverage includes cross-thread safe-handle PCM delivery, non-Copy queue lifetime
and exact drop counts, Send-but-not-Sync payload transfer, 200,000-item concurrent
FIFO transfer, and compile-fail examples for shared mutation, invalidated peeks,
consumer sharing, duplicated host ownership, reset while borrowed, and raw audio
calls without unsafe. No Miri/TSan execution or field qualification is claimed.

## Session-security primitives follow-up

`security.rs` implements SPEC §4.0–4.1 building blocks, now used by the opt-in
Rust PCM path; the legacy C constructor remains unauthenticated. `PendingBootstrap` binds an accept to its outstanding nonce
and PSK, recomputes/verifies the epoch, and consumes the transaction only on
success. `VerifiedRequest` means MAC-verified, **not** replay-admitted; its caller
must check active/cached nonces before responding. The host-side session manager now supplies fresh CSPRNG nonces, 24-hour
replay admission, simultaneous initiation and retries. The coordinator/bridge
now supplies bootstrap media routing into the authenticated Rust engine.

`SessionKeys` derives the epoch and dedicated control key with the exact two
BLAKE3 contexts. `ControlTx` emits counters 0–59,999 and refuses further beacons.
`ControlRx` verifies tags before changing its 64-bit replay bitmap, accepts unseen
recent counters, and rejects duplicate/expired control. CCF verification decodes
RS/sync/CRC before MAC checks and binds the full locally recorded transmit
counter. The low-level verifier accepts at most one CCF per expected counter;
transaction-aware verification now permits bounded authenticated ACK-refresh
duplicates without repeated control effects. PLCP callers must
perform physical sync/Golay validation before passing decoded beacon fields.
A token bucket permits an initial burst of 10 failures, then refills at 10/s;
PLCP and CCF share it. Backwards time cannot refill it. Only actual failed MAC
checks count as tamper; replay, channel corruption, and throttled attempts do not.
This limiter does not change listening/ARQ state. Bootstrap attempt limiting and
host telemetry/warning publication still need integration.

Explicit compatibility decisions needing confirmation before interoperating
with another implementation:

- Bootstrap 64-bit SipHash tags use little-endian bytes; the spec gives no tag
  byte order. Epochs are LE32 and CRC16 is big-endian as specified. A fixed
  fixture pins the chosen format; it is not an official V-RADM test vector.
- Exactly 128 counter steps are rejected as ambiguous (the algorithm text says
  +128, while the outage text permits only +127). Inferred negative/out-of-lifetime
  counters are rejected instead of clamped. Rejections alone do not invalidate
  a session, so unauthenticated input cannot force state reset. They report
  `CounterInference`, distinct from the trusted-outage `ResyncRequired` result.
- Extended outages must be detected by a trusted timer which calls `invalidate`;
  an 8-bit wire sequence cannot identify arbitrary gaps by itself. Invalidated
  receivers require a new bootstrap/context for both PLCP and CCF.

The spec's 8-bit tags remain lightweight filters and can collide. These helpers
are not strong application authentication or a completed protocol security audit.
They do not yet provide automatic secret-memory zeroization.

The crypto implementations come from the upstream [BLAKE3 KDF API](https://docs.rs/blake3/1.8.2/blake3/fn.derive_key.html),
[SipHash-2-4 API](https://docs.rs/siphasher/1.0.3/siphasher/sip/struct.SipHasher24.html),
and [constant-time comparison API](https://docs.rs/subtle/2.6.1/subtle/trait.ConstantTimeEq.html).
The old allocating BLAKE3 tree implementation was removed; SOTP still exposes
its original 28-byte digest API. No C ABI layout was changed.

## Host-side session manager follow-up

`SessionManager` handles one outstanding transaction and retains a fixed-size
nonce cache (default 128 entries). OS entropy uses `getrandom` and runs on the
host; it may block and must not run on the audio callback. Entropy failure has
no insecure fallback. Deterministic nonce sources exist only in tests. Cache
admission reserves space before changing sessions, rejects nonce reuse and fails
closed when full. It never evicts an unexpired or active nonce. A retired active
nonce is retained for a further 24 hours. Reset retains history; rebuilding the
manager/process does not persist it, so host persistence remains necessary.

Requests retry with identical wire bytes, at most three retransmissions. Late
polls emit at most one retry and never extend the original transaction lifetime.
The responder caches one accept while provisional and can resend it without
new entropy or a deadline extension. It becomes established only after successful
peer beacon verification; bad MACs do not confirm it. Established requests with
active/cached nonces are silently rejected (error/no outbound frame).
Simultaneous initiators compare nonces, with the larger remaining initiator;
the smaller yields transactionally. An exact tie emits nothing and expires.
Roles contain no phone/PBX assumptions. Manager-owned transmit counters persist
across beacons and refuse further transmission after 59,999 until a fresh
handshake; old accepts cannot complete that new transaction.

Implementation choices beyond explicit spec text:

- Nonce numeric comparison uses big-endian byte order; the spec does not state
  uint128 nonce byte order. This needs agreement before external interoperability.
- After the third retry, wait one final maximum listed RTO before timing out.
  Thus total FSK lifetime is 42,000 ms; MCS0 lifetime is 52,500 ms. Timer origin is
  request scheduling; the future media scheduler must account for queue delay.
- Provisional responder state distinguishes lost-accept retries from established
  session replay. Its confirmation is a successfully authenticated peer beacon,
  not a third bootstrap wire message.

The original manager tests pass wire frames directly between peers, including
an OS-CSPRNG smoke test. The handshake coordinator now adds full PCM negotiation
and confirmation; the authenticated engine provides queued context installation
and live PLCP gating. CCF transaction scheduling, platform audio/drain adapters, durable
nonce-cache persistence and automatic outage/rekey triggers remain outstanding. The 8-bit beacon confirmation inherits the spec's
lightweight integrity strength; it is not strong application authentication.

## Aligned bootstrap PCM payload follow-up

`BootstrapFskTransmitter` and `BootstrapFskReceiver` now bridge bootstrap wire
frames to/from 8 kHz i16 PCM. Payload length is exactly 256 bits × 80 samples =
20,480 samples (2.56 seconds). Bits are MSB-first with 1200 Hz mark/1600 Hz space,
matching existing PLCP conventions. Transmission uses precomputed tone tables
at peak amplitude 0.45; each bit contains an integer number of carrier cycles.
`render` returns payload sample count and fills the unused tail with silence.
The receiver accepts arbitrary chunk boundaries, uses an 80-sample scratch
buffer, and stops at the frame boundary with an explicit consumed count. It
emits at most one result until explicitly rearmed.

Silence/low-confidence symbols erase the payload. Complete payloads also pass
CRC16 and basic type/reserved-field validation before being handed to the host
session manager for authentication. Erasure thresholds are prototype choices,
not measured codec acceptance thresholds. Request and accept now round-trip
through this aligned PCM codec in tests and yield matching session epochs;
responder establishment still awaits a verified peer PLCP beacon.

This deliberately adds no preamble or guard to the 32-byte payload. SPEC §4.0
specifies payload timing but not enough bootstrap-specific acquisition framing
for a complete receiver. `at_frame_start`/`reset_to_frame_start` require an
externally supplied exact boundary. The opt-in profile below now supplies
acquisition; drift recovery, 16 kHz support, bootstrap MCS0, codec qualification
and platform device integration remain missing.
No existing C ABI, data PHY behavior, or wire frame layout changed.

## Opt-in bootstrap acquisition follow-up

[BOOTSTRAP_PROFILE.md](BOOTSTRAP_PROFILE.md) defines the project-specific
Barker-13 + unchanged 2-FSK bootstrap profile and its timing budget. New
`BarkerBootstrapTransmitter`/`BarkerBootstrapReceiver` wrappers render/acquire it
from streaming PCM. The bare-payload API still requires an exact boundary and
remains wire-compatible with its previous behavior. The profile is not enabled
by the live engine or claimed as an externally standardized bootstrap framing.

Acquisition uses fixed memory and preserves lookahead samples across callback
boundaries. It permits polarity reversal and restarts after payload success or
failure. A false preamble occupies at most one payload attempt; there is no
mid-payload reacquisition or timing-drift correction. Search costs 520 correlation
products per input sample; hardware callback CPU budgeting remains unqualified.
Bootstrap request/accept frames now reach session managers through this acquired
PCM path in tests. The opt-in Rust engine now gates application data on installed
sessions and authenticates peer PLCP. The host-owned coordinator now routes
provisional confirmation; platform device adapters and live CCF media scheduling remain.

## Authenticated Rust engine follow-up

[AUTHENTICATED_ENGINE.md](AUTHENTICATED_ENGINE.md) documents opt-in construction,
context handoff and ownership, generation/reset behavior, time accounting,
role/DIR mapping and remaining integration. The new constructor returns a boxed
engine to avoid large by-value Result temporaries exhausting test-thread stacks.
It supports only 8 kHz MCS 2/3. Existing C ABI constructors remain unchanged.

`SessionManager::take_established` moves existing counter owners, so a verified
bootstrap-confirmation beacon cannot become replayable when the audio engine
starts. Failed queue admission returns the transfer intact. The audio owner
applies queued contexts at outgoing burst boundaries and resets receive buffering
before using new keys. Replacements discard prior queued packets and ARQ state;
seamless transport-preserving rekey is still missing. Automatic handshake and
provisional-confirmation routing remain outside the engine.

The PHY now offers an authenticated transmit path and a receiver verifier hook.
The engine's opt-in path requires that hook to succeed before payload demodulation.
Guard energy disambiguates nominal/MCS3 layouts before MAC evaluation. Complete
guards are buffered before verification; MAC/replay state is not advanced on an
incomplete header callback. Rejected beacons cannot deliver canonical frames to
ARQ. Actual MAC failures update telemetry; physical corruption and replay drops
do not count as tamper. The legacy PHY and engine paths remain explicitly
unauthenticated for the original prototype harnesses.

## Remaining core gaps (M1 remains in progress)

| Area | Current limitation / next work |
| --- | --- |
| Session security | Opt-in Rust engine now verifies PLCP MAC/replay before payload/ARQ, using single-use established-session handoff. Legacy C constructor remains unauthenticated. Host-worker bootstrap/provisional confirmation is implemented; platform PCM/drain adapters, mutual readiness acknowledgment, acoustic control scheduling, outage/rekey coordination, C ABI exposure and field qualification remain. |
| Compact control and TDD | Compact frame serialization, authenticated request/response guards and separate aligned CCF/EOT/guard transmit/receive components exist. Standalone authenticated compact ACKs now run in the Endpoint full-duplex profile with phase-bank acquisition. Acoustic TDD scheduling/ownership, qualified acquisition and collision recovery remain absent. Current canonical feedback is suitable for the software/digital link harness, not proof of acoustic TDD compliance. |
| Modulation | MCS 0/1 fall through to the DQPSK path. MCS 4 currently uses 40-sample DQPSK slots (1,320 samples/frame), not the required 28+4 CP-OFDM slots (1,056 samples/frame). Implement these modes and qualify them independently. |
| Timing and codecs | Continuous DLL/Farrow recovery, CP tracking, codec-in-the-loop and TC-01–TC-11 qualification remain outstanding. No vocoder-resilience claim follows from these tests. |
| Configuration | 16 kHz and auto-rate adaptation are not fully wired through DSP. The unconditional-tanh defect is corrected; nominal conditioning is linear and the exceptional-only backstop is tested. Full signal/limiter qualification remains open. The transmit RMS ceiling and boundary-applied amplitude command are now wired into data-engine burst conditioning; separate control/bootstrap profiles retain their own amplitude rules. Endpoint now integrates drained-window MCS2→3 negotiation and sequence-boundary application using trusted host metric input. General adaptation remains open; legacy engine MCS commands still switch locally. |
| ARQ and packet delivery | Adaptive RTO and temporal context expiry still need work. Ordered reliable delivery is implemented. Structural fragment validation and host receive-queue backpressure are implemented (see below). Initial sequence is now explicit (default 0); authenticated negotiation is not implemented. Overlapping live packet ranges and extensions of delivered packets are rejected; conflicting duplicate content and broader ACK validation still need review. |
| Ownership/lifecycle | Safe Rust has exclusive queue endpoints and host/audio engine handles. Raw shared engine operations are unsafe and require caller-enforced roles. C callers must honor §10's ownership/quiescence contract. Queued reset preserves consumer ownership and host-side object cleanup. Miri/TSan validation remains outstanding. |
| SOTP | Staging/hash helpers and test-injected receive objects exist; there is no end-to-end RFC 6330 encoder/decoder transfer. |
| Platform | No shared-memory crate, gateway daemon, TCP-PEP, mobile project, or full platform E2E harness exists yet. |

The earlier worker's passing-suite claims preceded the unfinished round-3
challenge tests; they should not be interpreted as M1 or platform acceptance.
`TEST_INFRA.md` is a proposed platform test plan; its runner and platform tests
have not been implemented.

## Multi-endpoint direction

Preserve one symmetric packet/PCM engine for all endpoint pairs:

| Pair | Local adapters | Required transport behavior |
| --- | --- | --- |
| Phone ↔ PBX | Mobile audio/tunnel and AudioSocket/service adapter | Packet/stream endpoints on both sides |
| Phone ↔ phone | Mobile audio/tunnel at both ends | No dependency on a gateway or a fixed server address |
| PBX ↔ PBX | AudioSocket/service or routed-network adapter at both ends | Peer routing and service policy independent of call termination |

Separate three concepts in the next configuration revision: endpoint adapter
(mobile/PBX/test), negotiated session role (initiator/responder), and media
path capabilities (sample rate, duplex, codec). Assign wire DIR and initial
turn ownership from session roles, not device type. The spec's current DIR
labels and gateway-first turn rule will need an explicit revision; do not
silently reinterpret them in one adapter. The opt-in authenticated Rust engine
uses initiator DIR=1 and responder DIR=0 as an explicit project profile documented
in AUTHENTICATED_ENGINE.md. The legacy engine still uses DIR=0 in both directions;
interoperability agreement and negotiated turn ownership remain open.

Likewise, make local/peer tunnel addresses and allowed services explicit;
keep the existing 10.99.0.x and SSH/Mosh setup as a profile. Phone-to-phone
service hosting depends on OS capabilities and will need separate validation.

Suggested next sequence:

1. Finish M1 control authentication, ownership/lifecycle safety, ARQ delivery,
   and correctly implemented/qualified PHY modes.
2. Implement `vradm-shm` with actual file-backed mmap, exclusive SPSC endpoint
   handles, and subprocess restart/reattachment tests.
3. Build a shared endpoint/session layer and AudioSocket adapter with bounded
   sessions and protocol tests; qualify PBX ↔ PBX in software first.
4. Add mobile FFI/audio/tunnel adapters and TCP-PEP; run the original full E2E
   harness, then phone ↔ phone alongside phone ↔ PBX qualification.

## Verification

Run from repository root:

```sh
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p vradm-core --test reliable_audio_link
```

`reliable_audio_link` exercises MCS 2/3 symmetric bidirectional traffic through
PCM with 80/160/320-sample callbacks, sequence wraparound, first-burst loss,
feedback loss, duplicate suppression, and idle feedback termination. The
round-3 challenge also verifies 300 consecutive one-way packets over PCM.
Telemetry stress readers now assert cross-field relationships, not just
individual counter bounds. `receive_admission` checks malformed frames and
reassembly admission. The PCM harness also covers receive-queue saturation and
retrying polls with larger buffers. Existing allocation tests remain in the full suite.

Latest local results (2026-09-25): `cargo check --workspace --all-targets`
passed without warnings; `cargo test --workspace` passed **248 tests, 0 failed**
(241 runtime tests and 7 compile-fail doctests), including MCS retry/cooldown,
verified commit planning, emergency cancellation and allocation checks, CCF transaction
exclusivity, deadlines, duplicate semantics and allocation checks, idle confirmation loss
recovery, fresh retry counters, cancellation and retry allocation checks, bounded PCM queues,
drain tokens, concurrent bridge endpoints, queue-full installation retry,
allocation-free bridge callbacks, automatic PCM handshake
and engine handoff, forged/lost confirmation, lost accept, bootstrap failure-budget
recovery/reset checks, authenticated engine/PHY
traffic, session transfer and replay preservation, bootstrap acquisition, aligned
payload transport, session management, security, Rust ownership, queued resets, concurrent allocation checks, receive admission/
backpressure, and loss/bidirectional tests. The final run used `--locked --offline`. `git diff
--check` passed. No device, carrier, Asterisk, Flutter, or codec qualification
was performed in this continuation.
