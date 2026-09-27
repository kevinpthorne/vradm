# Integrated authenticated endpoint

`endpoint::Endpoint` binds the existing bootstrap coordinator, bounded PCM bridge
and authenticated engine into one Rust API. This is the first integrated
lifecycle slice in M1_PLAN: applications no longer construct session transfers,
sign bootstrap replies or manually route a chosen engine through a chosen bridge.
It supports 8 kHz MCS2/3 with explicit negotiated upshift and rejects automatic adaptation.
It now integrates streamed standalone CCF acknowledgments in an independent
full-duplex stream profile; see [LIVE_CCF.md](LIVE_CCF.md). The conservative live
MCS2→3 path and explicit emergency return to MCS2 are documented in
[LIVE_MCS.md](LIVE_MCS.md). Acoustic half-duplex
ownership remains absent.

## Ownership and use

Create `Endpoint::<128>::new(config, monotonic_ms)` off the audio thread. Call
`split()` once to obtain a unique EndpointHost and EndpointAudio. The default
host uses OS entropy; `split_with_entropy` supports platform RNGs and deterministic
tests. Production sources must supply fresh cryptographic nonces. The coordinator
moves into the host handle; keep that handle for the entire endpoint lifetime.
Re-splitting is rejected even after handles are dropped. This prevents silently
reconstructing a handshake manager without its replay history.

On a host worker, one side calls `begin(now_ms)`. Both sides call `pump(now_ms)`
regularly while capture/playback callbacks run. The worker handles incoming
bootstrap audio, retry timers, reply generation, confirmation and single-use
counter transfer. The transfer is retained if the engine queue is full. The
endpoint never lets a caller substitute another engine during that transfer.
The worker can allocate and use entropy; it is not an audio callback.

EndpointAudio provides `generate_audio` and `process_audio`. Capture returns a
dropped-sample count when handshake queues overflow; their offset tracking makes
those gaps visible to the coordinator. The audio owner serializes both callbacks.
When `generate_audio` reports `awaiting_device_drain`, obtain `playback_fence()`
and acknowledge that exact token with `acknowledge_played` only after the device
has consumed all preceding PCM. Tokens bind bridge identity and generation;
reaching the fence in a render callback does not itself prove device playback.

`pump` automatically installs the established session after that acknowledgment.
`ready()` becomes true only once the engine audio owner applies installation.
It means local readiness, not a mutual-readiness guarantee. Packet writes/polls
return VRADM_ERR_STATE before readiness. Afterwards the same handles exchange
packets through the authenticated engine; no manual session handoff is exposed.
Transmit RMS updates and telemetry are also available through the host handle.
The endpoint intentionally does not expose raw commands that could bypass its
coordinator reset or enable unnegotiated rate changes.

## Reset/rekey and failure handling

`reset(now_ms)` queues the engine reset, invalidates handshake routing/drain tokens
and resets the existing coordinator while retaining its nonce cache. It closes
packet admission immediately. If the engine command queue is full, reset returns
VRADM_ERR_QUEUE_FULL without resetting the coordinator; continue callbacks and
retry. Backward host timestamps are rejected before queue/state changes.

Buffered old-engine PCM is allowed to finish before queued reset and new handshake
playback. This avoids a deadlock where switching to handshake routing strands the
old audio ring and its pending reset. Already rendered samples in a physical device
cannot be retracted by this API. Continue rendering/pumping during graceful reset;
device adapters must account for their own queued media and bounded latency.
Both endpoints must reset/rebootstrap for coordinated rekey. The same or opposite
endpoint can initiate the next session. Old packet generations are discarded.

A device capture discontinuity after engine handoff requires host reset/rebootstrap;
automatic outage detection is not provided here. Sustained device/worker stalls
can exhaust existing handshake retries. Wrong PSKs and entropy/cache errors remain
errors from the underlying coordinator. Destroying the host/endpoint loses its
in-memory nonce cache; process-restart persistence is still external work.

## Verification and next integration

Tests exercise the public endpoint API for MCS2/3 bootstrap, bidirectional delivery,
maximum-burst reset/rekey with reversed initiator roles, lost bootstrap request,
lost data burst with exactly-once delivery, delayed/stale drain acknowledgments,
reset queue pressure, clock regression, unsupported profiles and retained nonce
history. Allocation instrumentation spans bootstrap routing, engine installation
and packet callbacks, excluding the host worker as required.

The harness delivers PCM synchronously before acknowledging playback; it is not
a real device adapter or acoustic half-duplex qualification. The endpoint now uses
streamed authenticated compact feedback and a drained-window MCS2→3 commit.
General MCS transitions, TDD ownership and collision recovery, automatic outage/
rekey coordination, mutual readiness and authenticated C bindings remain next
integration tasks. Missing PHY modes, SOTP and qualification still block full M1.
