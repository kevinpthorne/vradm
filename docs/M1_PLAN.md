# M1 completion plan — 2026-09-26

M1 is the complete core engine and PHY milestone in PROJECT.md, not merely the
existence of thirteen C exports or a passing unit suite. It is not close enough
to completion to justify a percentage or short ETA. Recent passes have built
useful isolated control components; their presence does not establish an
integrated modem. The following work remains, grouped by demonstrable outcome.

## Next integrated checkpoint (not full M1)

Two symmetric 8 kHz MCS2/3 endpoints should bootstrap, exchange authenticated
traffic, negotiate a rate change, recover a lost control response and reset/rekey
through one endpoint lifecycle. Use an in-memory PCM/device-drain harness first.
The endpoint must not require test code to manually sign replies, move counters
between disconnected policies, or assert that a sequence boundary was applied.
Then expose that lifecycle to C with explicit host/audio ownership.

Required integration:

1. Make the session/control owners and pending MCS state part of a single live
   endpoint lifecycle. Preserve existing bounded queues and single-use transfers.
2. Schedule actual request/reply audio, acquire incoming CCF boundaries and route
   decoded CCFs through the existing verifier to ARQ/control state. An aligned
   receiver alone cannot find a response in a live stream.
3. Retain receiver commit boundaries across fresh-counter retries. Sender payload
   changes require a verified ACK; receiver changes require a verified target-rate
   PLCP and the correct sequence boundary. Reject stale plans after reset/rekey.
4. Apply half-duplex ownership, EOT/guard and actual device-drain semantics, with
   bounded lost-turn/collision recovery. Make full-duplex versus acoustic behavior
   explicit. Session roles must support phone/phone and PBX/PBX without device-type
   assumptions; document any departure from the spec's gateway-first rule.
5. Define C integration for the authenticated lifecycle. The existing constructor
   still selects the unauthenticated prototype. Cover lifecycle, queue pressure,
   cancellation, outages and rekey in integrated PCM tests.

## Remaining full-M1 work

| Work package | Remaining implementation | Completion evidence |
| --- | --- | --- |
| Live control/session integration | The checkpoint above; mutual readiness, outage timers and rekey coordination | Automated two-endpoint authenticated lifecycle, control loss/retry and rate-switch tests through public APIs |
| Missing PHY modes | MCS0 pitch data frames, MCS1 speech atoms, proper MCS4 CP-OFDM; remove current DQPSK fallbacks | Mode-specific spec vectors and actual payload loopback for each implemented mode |
| Timing and DSP | DLL/Farrow/sample-slip recovery, CP tracking, 16 kHz handling, auto-rate metric measurement and dispatch, limiter/amplitude qualification | Drift/jitter/slip tests and measured signal limits; configuration demonstrably affects DSP |
| Reliability and lifecycle | Adaptive RTO, temporal context expiry, remaining ACK/duplicate validation review, Miri/TSan checks | Loss/outage/wrap/queue-pressure scenarios and supported concurrency tooling results |
| SOTP | Real RFC6330 object encoding, symbol transmission, receive reconstruction and hash verification | End-to-end lossy object transfer; staged/test-injected objects do not count |
| Qualification | Applicable TC-01–TC-11, codec-in-loop and real channel/device work | Recorded reproducible results; software loopback alone does not demonstrate vocoder resilience |

The latest amplitude change closes one concrete configuration gap: the configured
RMS ceiling now affects engine bursts, and SET_TX_PARAMS updates that ceiling at
burst boundaries. It does not complete the timing/DSP work package. Existing
normalization/limiter behavior still needs its separate spec qualification.

## Outside M1

Shared-memory IPC, the gateway daemon/AudioSocket service, TCP-PEP, mobile apps,
OS tunnel/audio adapters and full platform E2E are later milestones. Do not count
all platform work as an M1 blocker. Some external codec/device access is needed
for qualification, but the full mobile and gateway products are separate work.

Prioritize integrated behavior before adding more standalone control helpers.
Keep IMPLEMENTATION_STATUS as the verified handoff, and record exactly which
checkpoint is exercised rather than using the total test count as progress.
