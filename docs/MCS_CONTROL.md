# MCS negotiation policy

`mcs_control::McsNegotiator` adds SPEC §4.3 sender policy above the authenticated
CCF transaction guard. It borrows one endpoint's ControlTx and ControlRx exclusively
and performs no allocations, locks, or PCM work. Keep the policy object for the
session: recreating it loses its local cooldown/commit plan. Dropping it does not
clear an outstanding request in ControlTx. It is not wired into the live engine.

## Upshift attempts and cooldown

`request_upshift` validates a higher target in 0–4 and a positive, non-overflowing
RTO. It emits a request beacon at the current rate, advertises the target, and
expects an authenticated McsCommitAck with the requested target/yield semantics.
Current rate remains unchanged. RTO comes from the caller and must cover actual
airtime, device queues, and response delay; no dynamic estimator is supplied.

`poll` emits at most one retry after an expired deadline, always with a fresh
control counter. Delayed polling never produces a catch-up burst. The limit is
three total attempts: one initial request plus two retries. After the third
timeout it returns Aborted, restores requested MCS to current MCS, and starts a
ten-second upshift cooldown at the local abort decision. Ordinary `data_beacon`
traffic remains possible during cooldown. Another upshift is accepted exactly
when ten seconds have elapsed. Saturating elapsed-time calculation prevents
clock overflow from bypassing cooldown; backward mutation times are rejected.

Other upshifts and ordinary outgoing beacons are blocked while negotiating or
waiting to apply an accepted commit. This preserves the CCF guard's single
outstanding counter and avoids advertising the new rate early. Incoming peer
beacons can still be authenticated with `verify_peer_beacon` using the same
receive replay state and PLCP/CCF failure budget.

## Verified commit plan

`receive` accepts raw CCF codewords and authenticates them itself. Callers cannot
release the negotiation by supplying a fabricated ControlResponse. A matching
response records a McsCommit containing the old/target MCS and first sequence
`ACK_BASE + 1` modulo 256. Duplicates can refresh ACK fields but cannot change
that saved boundary. A reply at its exact deadline is too late; the next poll
schedules a fresh attempt. Forgery, malformed input and wrong-response semantics
never change the rate.

An accepted commit is a plan, not a modulation change. `current_mcs` stays old
until the trusted media scheduler applies the plan at the specified reliable
sequence boundary and calls `complete_commit`. The policy does not inspect payload
sequences or prove that boundary application occurred. Ordinary target-rate
beacons become available after this explicit completion. The plan survives the
CCF duplicate-response context's expiration; a verified commit is not undone
merely because a later duplicate can no longer be accepted.

`cancel` discards an attempt or plan without changing the current rate or erasing
an existing cooldown. `emergency_downshift` does the same and immediately updates
the policy's current rate to a lower MCS; no peer ACK is needed. The caller must
establish the trusted local channel condition (spec metric M < 0.60) before using
it and must apply the new rate to the media scheduler. These are trusted local
actions, never reactions to an unverified peer control message.

## Validation and remaining integration

Tests exercise fresh-counter retries, three-attempt abort, exact cooldown release,
data during cooldown, authenticated commit/duplicate boundaries across wrap,
forged and late responses, delayed polling, clock overflow/regression, cancellation,
emergency downshift, invalid requests, existing-request exclusion, and rekey
counter exhaustion. Allocation instrumentation covers commit, retry, cooldown,
ordinary data and cancellation.

The policy accepts logical MCS numbers 0–4; that does not implement their PHYs.
Aligned CCF modulation/decoding is available in [CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md).
Receiver-side metric admission and signed response construction are described below.
Still missing: CCF acquisition/live transport, commit response scheduling, sequence-aware application to live payload modulation,
PLCP-triggered receiver switchover, dynamic RTO, TDD scheduling, host events and
C ABI integration. The existing engine's direct MCS command remains a prototype
control and does not call this negotiator. M1 remains incomplete.

## Receiver-side commit response admission

`ControlRx::prepare_mcs_commit` now provides a guarded response constructor.
Use it **instead of** `verify_beacon` for an incoming upshift request, on the same
receive owner used for ordinary traffic. It authenticates the beacon and admits
its full inferred counter through the existing replay window and MAC-failure
budget. A delayed request older than a newer verified beacon cannot revive a
superseded control request, even if ordinary replay-window policy would admit it.

Trusted local `McsReplyParameters` supply ACK state, the desired response yield
flag, and the measured channel metric. Invalid/non-finite metrics, values outside
0–1 and invalid ACK bitmaps are rejected without consuming the request. Valid
requests require REQ_MCS > CUR_MCS and metric >= 0.85. Authenticated requests
refused for low metric, non-upshift or staleness still consume receive replay
state; the peer must send a fresh counter for another attempt. Forged requests
consume the existing shared MAC-failure budget without producing a response.

Success returns `McsCommitReply` with a signed codeword, full request counter,
target MCS and first sequence ACK_BASE+1 modulo 256. No caller-supplied counter
or separately retained key copy is needed. A yielding reply can be passed to
`CcfTurnTransmitter`; the aligned receive component still returns UnverifiedCcf
for sender-side MAC/transaction admission. A non-yielding reply is available for
future full-duplex use but is rejected by the half-duplex turn renderer.

This API does not schedule transmission, reserve a receive-rate change or switch
the demodulator. The future receiver scheduler must retain the selected commit
boundary across retransmissions, wait for an authenticated target-CUR_MCS beacon
and apply the boundary to actual sequences. Callers must discard obsolete replies
on rekey, outage, superseding requests or cancellation. A saved codeword can be
resent within its valid transaction; re-verifying the same request is a replay.
Logical MCS0–4 admission does not qualify those modulation modes or provide the
channel metric estimator. Tests cover response audio roundtrip, forgery/replay,
metric gates, delayed requests, full-counter wrap, invalidation, rate limiting
and allocation-free response construction.
