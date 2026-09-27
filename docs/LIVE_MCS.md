# Live MCS2/3 rate changes

The Rust Endpoint now negotiates an MCS2→3 upshift through its existing session,
streamed CCF audio and ARQ owners. This is a conservative 8 kHz full-duplex software
profile of SPEC §4.3, not automatic adaptation or acoustic TDD qualification.

## Host API

After `ready()`, the receiver's host supplies `set_channel_metric(m)` with a trusted
local value in [0,1]. Non-finite/out-of-range values are rejected. There is no
implicit estimate from prototype telemetry; absent a value, upshifts are refused.
M >= 0.85 admits a request. The supplied value remains effective until replaced
or reset. The caller must keep it representative of the channel; no measurement
or freshness estimator is implemented here.

The transmitter calls `request_upshift(3)`. It accepts only current MCS2 and target
MCS3. Return OK means the command was queued, not that either direction changed.
`rate_change_status()` reports Idle, Draining, AwaitingReply, AwaitingBoundary or
Cooldown. Telemetry reports applied TX/RX rates independently. A completed upshift
returns to Idle with TX rate 3. Another request while busy/cooling down is rejected;
queue-full failure leaves request admission available. Raw REQUEST_MCS commands
remain rejected by this endpoint profile. Automatic-rate configuration remains
unsupported. `emergency_downshift(m)` provides an explicit return to MCS2 when
trusted local M < 0.60; see below.

## Drain, negotiate and apply

A request first drains frames already admitted to ARQ, using ordinary old-rate
retries and authenticated ACKs. New packets stay in the bounded host queue. Once
both reliable and best-effort ARQ work is empty, an empty best-effort canonical
frame carries an authenticated CUR=2, REQ=3 beacon. It does not consume REL_SEQ.
This deliberately avoids rate transitions amid an outstanding reliable window;
it is more restrictive than the spec's general data-carrying request mechanism.

The receiver waits for the verified burst's yielding frame, requires the metric
threshold and an empty selective ACK map, and signs MCS_COMMIT_ACK with its own
ARQ boundary ACK_BASE+1. Its receive plan is retained across fresh-counter retries.
Only successfully signed replies create a plan. Older authenticated beacons may
consume replay-window admission but cannot replace the latest live burst context
or roll back a newer rate. Each PHY output batch belongs to one verified beacon;
buffered later bursts wait until the earlier frames have been consumed.

A matching authenticated CCF records the sender's plan only if the returned
boundary equals its next reliable sequence and its reliable window is empty.
An inconsistent authenticated boundary aborts into cooldown. Until the next
reliable frame starts at that boundary, TX remains at MCS2. Best-effort-only bursts
can continue at MCS2 because BE_SEQ is a different sequence space. The first
reliable burst at the saved boundary uses MCS3. No test-side 'complete commit'
call or manually signed reply is needed.

Receiver MCS3 payload demodulation is provisionally allowed only after an
authenticated target-rate PLCP and an existing receive plan. Payload/ACK admission
and applied RX telemetry wait for a reliable sequence in the forward eight-frame
window from the saved boundary, including wraparound. This allows loss of the
first frame without stranding the remaining window. Canonical mode bits must
match the verified PLCP. Old-rate reliable traffic at/after the boundary abandons
a pending receive plan, allowing recovery after the sender aborts. A later
verified lower-rate header permits the spec's unilateral receive-side downshift;
the sender's explicit emergency API supplies that lower-rate announcement.

## Emergency return to MCS2

`EndpointHost::emergency_downshift(m)` requires a finite trusted local metric
0 <= M < 0.60. It returns OK when queued. It can return MCS3 to MCS2 or cancel a
pending MCS2→3 upshift while already at MCS2. It does not change the independently
supplied receive-side admission metric or the reverse direction's commit plan.
A second emergency or new upshift is rejected while the emergency is queued;
queue-full failure leaves the endpoint state intact. `rate_change_status()` still
reports the upshift lifecycle; applied TX/RX rates are reported in telemetry.

The audio owner applies the command at the next burst boundary. It cancels the
outgoing control transaction and partial CCF acquisition, clears the wait for a
peer reply, and switches TX to MCS2. Reliable in-flight and queued packets survive,
so the next burst retransmits unacknowledged frames with a fresh lower-rate beacon.
An existing upshift cooldown is preserved. Late replies to the canceled transaction
cannot apply its old plan. Reverse-direction replies still receive scheduling
priority, and previously rendered or device-queued audio is allowed to finish.

If idle, an empty best-effort canonical frame announces CUR=REQ=2 without waiting
for an application packet or requiring a commit reply. If that idle announcement
is lost, the next data burst repeats the lower-rate header; there is no separate
idle announcement retry timer. The receiver changes rate only after an accepted
fresh authenticated lower-rate PLCP. This is explicit host-driven adaptation,
not a channel measurement or automatic adaptation loop.

## Retries and lifecycle

The existing fixed 12-second sample-clock transaction timeout covers each request.
There are three total fresh-counter attempts; after the third timeout the sender
stays at MCS2, releases queued data and enters a ten-second cooldown. A standalone
ACK cannot substitute for MCS_COMMIT_ACK. Lost or forged commit audio cannot
release the old rate. Pending peer replies take priority over local waits.

Reset/rekey discards sender requests, receive plans, diagnostic counts and the
supplied metric. Both directions restart at the configured startup rate. Existing
bootstrap/device-drain and graceful buffered-audio reset behavior is unchanged.
All negotiation callback state is fixed-size and uses the existing command queue;
allocation instrumentation covers handoff, negotiation, subsequent data/ACKs
and emergency downshift.

## Verified scope and remaining work

Public endpoint PCM tests cover success, lost/forged commit, low/absent metric,
three-attempt abort/cooldown, queued data release, BE before REL, draining an active
window, the 255→0 boundary, independent TX/RX rates, command queue pressure and
reset of an accepted plan/metric. A separate test rejects a validly authenticated
MCS3 burst when no receiver commit exists. PHY regression coverage checks two
buffered bursts retain their own verified-beacon context. Emergency tests cover
idle announcements, invalid metrics/queue pressure, completion of buffered PCM,
lost data/ACK retry at the lower rate, pending/accepted upshift cancellation,
subsequent renegotiation, command ordering and preservation of cooldown.

Remaining: general transitions and missing PHY modes, automatic channel metrics,
adaptive RTO, acoustic TDD/device-drain turn
ownership, mutual readiness/outage/rekey coordination and authenticated C exposure.
Software PCM success does not establish codec, timing-drift, CPU or device
qualification. See M1_PLAN.md for the larger milestone.
