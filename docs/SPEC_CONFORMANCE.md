# Focused spec conformance audit — 2026-09-26

Reference: [SPEC.md](SPEC.md), v3.8.10. This is a focused source review of the
implemented runtime profiles and known M1 gaps, not a completed TC-01–TC-11
acceptance audit. The specification has not been edited to match the code.
Passing tests establish the tested behavior; they do not waive requirements.

There are three different surfaces: legacy `vradm_create`/Rust `new`, opt-in
`new_authenticated`, and the newer `Endpoint`/`new_ccf_endpoint`. Statements about
the authenticated Endpoint must not be applied to the legacy C entry point.

## Known mismatches and incomplete requirements

| Area / spec reference | Current behavior and evidence | Classification / required action |
| --- | --- | --- |
| Authenticated default and lifecycle, §4/§10 | `vradm_create` still constructs the legacy unauthenticated engine. The authenticated endpoint exists only in Rust. [c_abi.rs](../vradm-core/src/c_abi.rs), [endpoint.rs](../vradm-core/src/endpoint.rs) | Incomplete integration. Add an explicit authenticated C lifecycle; do not describe the legacy constructor as secured. |
| MCS0/1/4 data PHY, §3.2 | Live MCS0/1 data fall through to DQPSK. MCS4 uses 40-sample DQPSK slots instead of 28+4 CP-OFDM. Pitch CCF support does not implement MCS0 data frames. Endpoint rejects these modes. [phy.rs](../vradm-core/src/phy.rs) | Unimplemented modes plus misleading legacy fallback. Implement/qualify or reject unsupported selections. |
| MCS commit, §4.3 | Endpoint now integrates drained-window MCS2→3 requests, authenticated streamed commit replies, retained receive plans and REL_SEQ/target-PLCP application. It rejects raw rate commands; older constructors still allow direct local switches. [LIVE_MCS.md](LIVE_MCS.md) | Restricted integrated profile. An explicit trusted-metric emergency return to MCS2 is integrated. General transitions and automatic metrics remain incomplete. |
| Acoustic TDD, §5 | Endpoint assumes independent full-duplex streams. No gateway/master token ownership, SILENT_LISTEN/backoff or collision scheduler. CCF TX emits EOT/guard, but streaming ACK admission does not wait for aligned EOT/guard completion. Data bursts lack acoustic EOT/guard scheduling. | Restricted profile, not acoustic compliance. §5 explicitly permits TDD-disabled full duplex; that exception does not qualify acoustic paths. |
| Burst/packet extent, §5.1 | Engine can emit eight data frames and accept 296-byte packets. Acoustic spec examples constrain turns to seven frames and use 256-byte datagrams. [engine.rs](../vradm-core/src/engine.rs) | Prototype extension. Do not carry the eight-frame setting unchanged into a seven-frame acoustic scheduler. Eight-entry ARQ storage alone is not proof of seven-frame turn compliance. |
| RTO, expiry and clocks, §5.1/§6.3 | Endpoint compact requests use fixed 12-second deadlines on sample counters. Older engine uses a fixed burst-size wait. No adaptive RTT estimator, negotiated RTO profile or complete context-expiry/outage policy. | Incomplete timing policy. Sample progress is not elapsed wall time when a device stalls. |
| Counter limit/rekey, §4.0 | Counters stop before 60,000; exhaustion closes authenticated traffic. Reset/rebootstrap is explicit and discards old packet/ARQ generations. No automatic seamless transport-preserving rekey. | Partial implementation. Safe closure meets no-wrap intent but is not the required seamless automatic behavior. |
| Sample-rate/drift, §3 | Endpoint supports 8 kHz only. Legacy config accepts 16 kHz without a complete 16 kHz DSP implementation. DLL/Farrow and CP timing recovery remain absent. | Incomplete DSP. Reject unsupported profiles in production-facing paths until implemented. |
| Limiter scoping, §3 amplitude/IMD rules | **Corrected in follow-up:** nominal samples pass unchanged; the specified tanh branch is used only above 0.45 FS. Double-precision metering avoids energy overflow and boundary-rounding activation. [phy.rs](../vradm-core/src/phy.rs), [conditioning.rs](../vradm-core/tests/conditioning.rs) | The unconditional-compression mismatch is corrected. Conditioner-only linearity/carrier-error tests pass; complete TC-10c EVM, activation-rate and device qualification remain open. The specified exceptional formula has a boundary discontinuity when joined to the linear region; clarification is still appropriate. |
| Metrics/adaptation, §3/§6 | Initial telemetry uses prototype SNR/metric values; no qualified metric estimator drives automatic adaptation. Endpoint rejects auto-rate config and requires an explicit trusted host metric for live commit admission. | Incomplete implementation. Initial values must not be treated as measured link quality or fed into commit policy as evidence. |
| SOTP, object-transfer requirements | Staging, hashing and test-injected receive state exist; no end-to-end RFC6330 symbol encoding/transport/reconstruction. | Incomplete feature. API presence is not implemented object transfer. |
| Qualification and lifecycle tooling | Synthetic loopback/allocation/concurrency tests pass; no completed codec/device/drift/CPU-budget suite or Miri/TSan acceptance. | Unverified requirements, not evidence of conformance. |

## Explicit project profiles and defensive interpretations

These choices need interoperating peers to agree and, where appropriate, a future
spec amendment. They are not silent changes to the normative document.

| Choice | Spec versus implementation | Consequence |
| --- | --- | --- |
| Direction identity | §3 dither DIR names Gateway→Client as 0 and Client→Gateway as 1. Authenticated code uses responder→initiator as 0 and initiator→responder as 1. Legacy code uses 0 both ways. | Intentional symmetric-endpoint profile for phone↔phone/PBX↔PBX. It agrees with the named mapping only when the client is initiator; gateway-initiated interoperability needs explicit agreement. It does not solve initial acoustic token ownership. |
| Bootstrap acquisition | §4.0 gives a 32-byte, 2.56-second FSK payload but not a complete acquisition waveform. Project prepends a 520-sample Barker sequence, yielding 2.625 seconds. [BOOTSTRAP_PROFILE.md](BOOTSTRAP_PROFILE.md) | Explicit framing extension. Payload bytes and MAC formulas remain unchanged; both peers need this framing profile. |
| Provisional confirmation recovery | Coordinator-generated initiators arm three bounded fresh-counter idle probes, at least six rendered seconds apart. | Added recovery profile, not the bootstrap RTO or a mutual-readiness protocol. See AUTHENTICATED_ENGINE.md. |
| Counter inference | §4.1's formula permits delta=128 as positive and clamps under/overflow to 0/65535. Code rejects exact half-cycle ambiguity and out-of-range candidates instead of aliasing them by clamping. | Intentional stricter rejection. Reconcile the normative formula before claiming bit-for-bit inference conformance; do not remove replay safeguards merely to match the pseudocode. |
| CCF waveform/acquisition | Spec fixes pitch alphabet, 400-sample symbols and final-280-sample NCCF window. Project chooses high-nibble-first packing, phase-zero sine/taper details and an EOT amplitude interpretation. Streaming RX adds ten phase hypotheses with a 40-sample hop and requires detectable sync. [CCF_PCM_PROFILE.md](CCF_PCM_PROFILE.md), [LIVE_CCF.md](LIVE_CCF.md) | Explicit waveform/detector profile, not a qualified clock tracker. Channel-valid candidate timing is not exact frame-end timing. Erased sync cannot be acquired by this search even when aligned RS could recover it. |
| Drained-window MCS commit | Live MCS2→3 requests wait for empty ARQ state and use an empty canonical request; target-rate payload waits for the next REL_SEQ. BE-only traffic can remain at the old rate. Older verified beacons cannot supersede live receive context. [LIVE_MCS.md](LIVE_MCS.md) | Conservative restriction of the general §4.3 handshake, not general automatic adaptation. Host-supplied metric persists until replaced/reset; no measurement/freshness estimator is supplied. |
| TX command mapping | SET_TX_PARAMS currently interprets param_f32 as RMS ceiling; other fields have no effect. Applied settings survive link reset. [TX_AMPLITUDE.md](TX_AMPLITUDE.md) | API/profile convention filling underspecified command details, not support for every advertised control setting. |
| Nonce persistence | Bounded nonce cache survives Endpoint reset while its host lives; cache exhaustion refuses admission. Destruction/restart loses the cache. | Safe bounded-memory behavior, but durable replay-history retention is not implemented. |

## Implemented requirements with scoped evidence

The differences above do not mean every wire format has diverged. Source and
existing tests cover the specified BLAKE3 context strings/material, SipHash control
preimages, CRC fields, full-counter CCF binding, 64-entry replay window, no-wrap
counter limit, single-outstanding transactions and authenticated response admission.
These still use the spec's lightweight MAC8 integrity model, not strong payload
authentication; application security remains at SSH/Mosh as the spec intends.

The CCF payload occupies 32 pitch symbols/12,800 samples; its aligned decoder uses
only the final 280 samples per symbol, marks erasures without holding old symbols,
and waits through the frame before failure. EOT TX uses 1400/1800 Hz for 1,200
samples with a 1,200-sample guard. Normal PLCP/MCS3-specific timing is tested.
Bootstrap/session ownership and callback allocation behavior have dedicated tests.
None of this substitutes for codec/field qualification of the experimentally gated
MCS2/3 modes; enabling them in a software harness is not production enablement.

## Priority and decision record

1. Keep Endpoint explicitly scoped to the tested full-duplex software profile.
2. The unconditional-limiter mismatch is corrected with nominal linearity,
   carrier-error and exceptional-peak safety tests. Complete modem TC-10c work
   and reconcile the exceptional equation's discontinuous boundary if needed.
3. Extend the integrated drained-window MCS2→3 path to general transitions, acoustic scheduling and C exposure.
4. Complete missing modes, timing, SOTP and acceptance work per M1_PLAN.md.
5. Ratify direction/bootstrap/CCF profile choices before external interoperability.

The original audit did not change runtime code or SPEC.md. Its limiter follow-up
and live MCS follow-ups change the runtime as recorded above. IMPLEMENTATION_STATUS.md contains the
latest verified suite count; this audit still does not establish full conformance.
