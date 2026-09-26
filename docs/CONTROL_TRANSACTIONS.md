# Authenticated CCF transaction state

The security module now provides a fixed-memory request/response guard for
SPEC §2.2 and §4.1. It works on decoded 16-byte CCF codewords. It is not yet wired
into the PCM engine: MCS0 CCF modulation/acquisition and media scheduling remain
unfinished. No automatic MCS change or TDD grant is enabled by this addition.

## Request ownership

`ControlTx::begin_control(ControlRequest, now_ms)` emits a MAC-bearing beacon and
records its full local 16-bit counter, expected CCF command, target MCS, yield bit,
and absolute deadline. Commands are typed as StandaloneAck, McsCommitAck or TddGrant.
The deadline must be strictly later than trusted monotonic `now_ms`. The caller
must choose an appropriate RTO; this primitive does not estimate one.

While a request is awaiting its first matching response, both `begin_control`
and ordinary `beacon` return OutstandingRequest without consuming a counter.
This deliberately prevents another beacon from replacing the counter against
which the response is inferred. A correct response, trusted local rejection,
or timeout releases the guard. Repeating the request after timeout consumes
a fresh counter; it does not reuse an old beacon/tag.

`expire_control(now_ms)` clears the context at the exact deadline and reports
that expiration once. `reject_control(now_ms)` clears it by an explicit trusted
local decision. Neither should be used as a reaction to forged traffic. Clock
regression returns ClockWentBackwards. A bare `beacon` has no time argument, so
call `expire_control` to release an expired pending request before using it.
The existing 60,000-counter rekey limit still applies.

## Response admission and duplicates

Call `ControlRx::verify_control_response` with the same endpoint/session's Tx
owner, received codeword, physical erasure positions, and trusted time. It obtains
the counter from the pending request; there is no wire/caller counter argument.

SYNC, RS and CRC validation precede MAC work. CCF marker, MCS range, command class
and the seven-bit ACK map are validated. MAC checks use the existing shared
PLCP/CCF failure budget. Only a verified frame whose command, target and yield bit
match the recorded expectation can commit. An authenticated but unexpected
response returns UnexpectedControl without consuming the transaction's semantic
state; malformed input, bad tags and channel errors also leave it uncommitted.
Trusted time still advances and can expire the request.

`ControlResponse.frame` contains verified ACK information. Every accepted response
may feed those ACKs into ARQ. Apply control actions only when `apply_semantics` is
true. Authenticated duplicates remain admissible until the original deadline or
next successful outgoing beacon/request, but return false for that flag.
Duplicates are MAC-checked again, not trusted merely because one reply succeeded.
They do not extend the deadline.

For McsCommitAck, `commit_sequence` is the first accepted ACK_BASE plus one modulo
256. A later authenticated duplicate may carry newer ACK fields; it cannot move
that saved switchover boundary. For other command types the boundary is None.
The scheduler still must implement sequence-aware MCS switching, channel-metric
admission and TDD ownership. McsNegotiator now supplies sender retry/cooldown
policy above this guard. This module changes no media state itself.

The older `verify_ccf(expected_counter, ...)` API retains its strict one-response
behavior for low-level callers. Do not mix it with transaction-aware verification
for the same request. Both paths use the same replay watermark and failure budget.
Tx/Rx owners are not cloned; their transaction/replay state stays with them during
an existing session transfer. Outage invalidation rejects further responses and
requires replacement session state.

## Validation and limits

Tests cover overlapping requests, fresh counters after expiry/rejection, exact
and backward deadlines, semantic mismatch, duplicate ACK updates, fixed commit
boundary across sequence wrap, stale replies after an 8-bit counter wrap, shared
failure throttling, channel-error separation, and outage invalidation. Allocation
instrumentation covers 300 transactions and duplicate responses across wire wrap.

MAC8 retains its documented lightweight integrity strength. Different counters
produce different MAC preimages but their eight-bit tags can collide; the wrap
regression test deliberately uses non-colliding fixtures. This is not strong
application authentication. Aligned PCM modulation/decoding is now available in
[CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md). CCF acquisition/media scheduling,
live ARQ/control dispatch, dynamic
RTO, host events and C ABI exposure remain separate work.
[McsNegotiator](MCS_CONTROL.md) now supplies the three-attempt sender policy,
cooldown and deferred commit plan, without live media switching. M1 is not complete.
