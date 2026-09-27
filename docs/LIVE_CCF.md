# Live compact feedback profile

Endpoint now uses `vradm_engine::new_ccf_endpoint`, a separate opt-in authenticated
8 kHz MCS2/3 software/full-duplex profile. Reliable data opens a StandaloneAck
transaction on the existing ControlTx owner. Incoming verified PLCP establishes
the peer's full request counter; processing its final reliable frame schedules
an ACK snapshot from ARQ. The receiver signs against that verified counter and
renders the existing CCF/EOT/guard waveform. No new wire preamble is added.

The sender scans incoming PCM while its request is outstanding and passes only
channel-valid CCF candidates to the same ControlRx owner used for PLCP. MAC,
counter, deadline and expected-command checks precede ARQ ACK ingestion. Success
closes that transaction and releases the sender's wait. Canonical ACK-only bursts
are disabled in this profile; canonical payload frames still carry piggyback ACKs.
Idle bootstrap-confirmation probes remain available and still use canonical frames.
The prior authenticated constructor and legacy C constructor retain their behavior.

## Acquisition

`CcfStreamReceiver` evaluates one 400-sample pitch window every 40 samples and
maintains ten symbol-phase hypotheses. Each searches for the four voiced nibbles
of SYNC D391, then collects the remaining 28 symbols using the existing NCCF
and RS byte-erasure handling. Complete RS/SYNC/CRC validation precedes candidate
output. Adjacent phase hypotheses are collapsed after a channel-valid candidate,
so one forged physical CCF does not multiply MAC failures across those hypotheses.
A new outgoing request/reset clears partial receive acquisition.

This is bounded fixed-memory software acquisition, not a qualified timing loop.
The sync prefix itself must be detectable; unlike the aligned decoder, the search
cannot recover an erased sync prefix using RS. Tests cover non-grid input offsets,
arbitrary chunks, payload erasures, successive turns and partial-capture reset.
Real codec/filter/drift behavior and callback CPU budgets remain unqualified.

Candidate arrival does **not** establish an exact physical frame edge or EOT/guard
completion. This profile assumes independent full-duplex streams. It does not
use acoustic markers to transfer a half-duplex token, and it does not claim AEC,
echo, collision or room-reverberation handling. The aligned CcfTurnReceiver remains
available for future boundary-aware scheduling but does not gate this profile's
ACK admission. Acoustic TDD needs separate ownership and playback-drain integration.

## Scheduling, timeouts and reset

A reliable request uses a fixed 12-second timeout on the maximum of render and
capture sample clocks relative to session installation. The value accommodates
the current software burst/CCF durations; it is not a measured adaptive RTO or
a guarantee for stalled device queues. Expiry releases the request and permits
ARQ retry with a fresh beacon counter. There is no second legacy wait interval
after that expiry. Pending compact responses take priority over the local wait,
allowing both full-duplex directions to acknowledge simultaneous reliable bursts.

CCF rendering counts as an active audio burst for command/session boundaries.
Reset clears acquisition, pending reply context and diagnostic counts after the
active burst drains. The bound Endpoint's graceful reset path handles data and
CCF bursts alike. Failed authentication cannot acknowledge data, release the
request or cancel confirmation recovery. Physical failure does not count as a
MAC failure. Application authentication remains at SSH/Mosh; the small control
MAC is only the spec's lightweight integrity filter.

`EndpointHost::ccf_counts()` reports compact ACKs scheduled and authenticated
responses accepted since reset. These diagnostics are separate from canonical
frame telemetry; scheduled does not mean physically played. They are not added
to the legacy C telemetry layout. This profile rejects raw REQUEST_MCS
commands rather than allowing a local switch around negotiation. SET_TX_PARAMS
still controls data-burst conditioning; CCF has its own documented amplitude.

## Verification and remaining integration

Public endpoint tests prove actual compact feedback rather than merely packet
arrival: one-way success has one data burst, one compact ACK and no canonical
ACK-only burst. Dropped and CRC-valid forged replies force fresh-counter retries
with exactly-once delivery. A forged reply increments tamper once; silence does
not. Existing bidirectional, bootstrap-loss, maximum-burst reset/rekey, nonce-cache
and device-fence tests run through this profile. Callback allocation instrumentation
now asserts compact ACK transmission and reception in both directions.

[LIVE_MCS.md](LIVE_MCS.md) describes the integrated drained-window MCS2→3
request, retained receive plan, retries and sequence-boundary application. Still missing: acoustic TDD scheduling, acquisition/CPU/
codec qualification, adaptive timing, automatic outage/rekey and authenticated C
exposure. This is progress within the integrated checkpoint, not full M1 completion.
