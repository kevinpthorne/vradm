# Opt-in authenticated PLCP engine path

`vradm_engine::new_authenticated(config)` returns a boxed Rust engine for 8 kHz
MCS 2/3. It accepts/emits no data PCM until an established session is transferred
into it. Existing `vradm_create` and `vradm_engine::new` retain their legacy
unauthenticated prototype behavior; no C structure or function signature changed.
This is the spec's lightweight PLCP integrity/replay filter, not application
payload authentication or encryption. SSH/Mosh still provide those properties.

## Establishment and ownership

1. Run bootstrap with `SessionManager` on the host, using the intended PSK and
   OS entropy. The engine consumes negotiated keys, rather than deriving a
   session from its config PSK by itself. Profile selection and audio routing
   must be explicit; see BOOTSTRAP_PROFILE.md.
2. Confirm the responder with a physically decoded, authenticated peer beacon.
   Only established contexts can be transferred. The tests do this with the
   PHY verifier. `HandshakeCoordinator` now automates this routing on a host
   worker; see [HANDSHAKE_COORDINATOR.md](HANDSHAKE_COORDINATOR.md).
3. Call `SessionManager::take_established(now_ms)` once. The non-cloneable
   `SessionTransfer` moves the **existing** ControlTx and ControlRx owners,
   preserving their counters, replay bitmap and MAC-failure budget.
4. Pass the transfer to `HostHandle::install_session`. On queue-full or wrong
   engine mode, the error returns `(code, transfer)` so ownership can be retained
   for retry. On success, queue application packets **after** this call.
5. The audio owner installs it at a transmit-burst boundary. The host observes
   readiness with `authenticated_ready`. No waits, locks, allocations or frees
   are required in the installation/audio callback path.

The manager retains nonce history and context for rekey admission after transfer,
but cannot transmit, verify, or transfer that control owner again. The audio
side extends the handed-off monotonic time with captured sample count (8 samples
per millisecond). The host must supply correctly paced audio and coordinate
trusted clock outages; wall-clock outage detection is not implemented here.

Installation is a new local packet generation, like queued reset: prior queued
packets and ARQ/reassembly state are discarded. Later commands and packets are
preserved. It does not provide transport-transparent rekey yet. Installation
finishes the active outgoing burst, clears partial old receive PHY state, and
never switches keys in the middle of that outgoing burst. Reset removes the
installed context and returns the authenticated engine to its closed state.
At counter exhaustion it also closes, reports not ready, and requires a fresh
handoff; it never falls back to a zero-MAC beacon. Host rekey scheduling remains
outstanding. Initiator transfers created by HandshakeCoordinator also carry a
bounded idle-confirmation recovery policy: up to three fresh beacon probes,
at least six seconds apart on rendered samples. A due probe uses outgoing data
when available or one empty best-effort frame. A verified peer beacon cancels
remaining probes; active bursts finish normally. Direct SessionManager transfers
and responder transfers do not arm retries. This project profile does not change
the bootstrap request RTO schedule or provide mutual readiness acknowledgment.

## PHY processing

Transmitted PLCP headers carry the session's actual monotonic-counter MAC.
Received headers undergo sync, confidence, Golay and physical-layout checks
before MAC/replay admission, then enter payload demodulation only if admitted.
MCS 3 is selected using its immediate FSK energy, rather than interpreting a
one-bit-shifted header as the nominal guard layout. Verification waits for the
complete guard interval, so an incomplete callback cannot consume a replay bit
and then verify the same beacon again.

Rejected beacons cannot produce data frames or ingest ACKs into ARQ. Actual MAC
failures increment engine `security_tamper_detected`; physical erasures, replay
rejections and rate-limited attempts do not. Host warning/event publication is
not implemented. Payload CRC/FEC still operate as before; PLCP tags do not cover
the canonical payload or its ACK fields.

The opt-in Rust profile assigns dither DIR=1 to the negotiated initiator and
DIR=0 to the responder; each receiver uses the peer's direction. This is the
explicit role-based interpretation needed for phone/phone and PBX/PBX symmetry.
It is not silently applied to the legacy constructor (which still uses DIR=0
both ways), and external interoperability needs agreement on this mapping.

## Validation and remaining integration

Tests cover bidirectional MCS 2/3 PCM, a lost burst retransmitted with a fresh
beacon counter, forged and replayed headers, pre-handoff replay history, guard
splits, physical erasures, queue-full transfer retry, burst-boundary replacement,
reset lockout, exhausted-counter closure, unsupported profiles, non-cloneable
transfers, and allocation-free installation/PCM callbacks.

[Control transaction guards](CONTROL_TRANSACTIONS.md) now bind CCF responses to
local requests and separate duplicate ACK updates from one-time control effects;
they are now connected for standalone ACKs in the separate Endpoint/
new_ccf_endpoint full-duplex profile; see [LIVE_CCF.md](LIVE_CCF.md). The original
new_authenticated constructor retains its canonical-feedback behavior.

[HandshakeBridge](HANDSHAKE_BRIDGE.md) now supplies bounded PCM queues, worker
pumping and device-drain-gated engine routing. Still missing: platform audio/drain
adapters, mutual readiness acknowledgment, authenticated CCF transaction scheduling,
negotiated MCS changes,
automatic outage/rekey triggers, C ABI exposure/selection, durable nonce history,
codec/hardware/drift qualification. Bootstrap verification throttling is provided
by SessionManager with a separate persistent host-side failure budget.
