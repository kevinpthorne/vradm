# Opt-in Barker bootstrap acquisition profile

This project profile fills an acquisition gap in SPEC §4.0. It is **not a claim
that SPEC v3.8.10 fully specifies this framing**, and it is not enabled in the
existing C-ABI engine. Both endpoints must select the same profile out of band.
The existing bare-payload codec and all 32 bootstrap wire bytes remain unchanged.

## Waveform

At 8 kHz, send these samples consecutively:

| Segment | Samples | Duration |
| --- | ---: | ---: |
| Existing §4.2 Barker-13 dual-chirp preamble | 520 | 65 ms |
| §4.0 32-byte bootstrap payload, 100-baud 2-FSK | 20,480 | 2,560 ms |
| Total | 21,000 | 2,625 ms |

There is no Golay PLCP header or gap between the preamble and bootstrap payload.
Bits are MSB-first; mark is 1200 Hz and space is 1600 Hz, following existing PLCP
conventions. Both waveform segments use peak scale 0.45. Nonces, tags and CRC
retain the byte orders documented in IMPLEMENTATION_STATUS.md/security.rs.

Guard silence is the scheduler's responsibility, not part of the frame. Allow
at least the spec's 150 ms turnaround guard before replying on a half-duplex
path. A request/response pair takes 5,250 ms of audio; one 150 ms turnaround and
500 ms processing allowance give 5,900 ms, within the existing 6,000 ms initial
FSK RTO. Extra media buffering/queuing is not included and may require revisiting
that budget. Start the transaction clock when scheduling its actual transmission.
Do not add an authenticated control header before keys have been established.

## Streaming API

- `BarkerBootstrapTransmitter::render` writes arbitrary PCM chunks, reports the
  payload/profile sample count, and zeroes unused output space.
- `BarkerBootstrapReceiver::push` searches for the preamble without requiring a
  callback or symbol boundary. It returns consumed input and at most one event.
  Resubmit any unconsumed suffix to process following frames.
- The receiver automatically searches again after success or payload failure.
  `reset` discards partial input after a trusted stream discontinuity.
- Send successful wire frames to the **host-side** `SessionManager::receive`.
  Physical decoding and CRC success alone never authorize session/ARQ changes.
  Keep OS entropy and transaction management off real-time audio callbacks.

The receiver uses a 520-sample ring and normalized absolute correlation, with a
prototype 0.85 threshold. Absolute correlation allows cable polarity inversion;
the threshold rejects the approximately 0.75 negative sidelobe of the existing
preamble. Four samples of local-peak lookahead are retained and fed into the FSK
payload decoder, preventing callback boundaries from discarding payload samples.
Search work is bounded by input length (520 correlation products per search
sample); payload decoding uses the existing fixed-memory symbol receiver.

## Limits and validation

Tests cover all 80 symbol offsets, callback sizes from one sample to 511 samples,
preamble-end callback splits, polarity reversal, quarter-amplitude signal plus
small deterministic background noise, consecutive frames after corruption, reset,
request/accept acquisition into session managers, and zero heap allocations/frees.
These are synthetic tests, not codec, CPU-budget, clock-drift, or hardware
qualification. The responder still awaits authenticated peer PLCP confirmation.

A false preamble locks the receiver to one bounded 2.56-second payload attempt.
The receiver does not search for another preamble during that attempt; later
handshake retries recover after it fails. Lost/inserted samples or continuous
clock drift may invalidate a payload. No DLL/Farrow recovery, 16 kHz mode, or MCS0
bootstrap is implemented here. SessionManager throttles bootstrap MAC failures
with ten burst credits and a ten-per-second refill; CRC failures spend no credit.
The host-owned [HandshakeCoordinator](HANDSHAKE_COORDINATOR.md) now schedules
bootstrap PCM and provisional confirmation. Durable nonce history, real-time
device adapters and drain callbacks for the PCM bridge, authenticated
CCF transactions and outage/rekey handling remain separate work. Established
session handoff and authenticated PLCP gating are available in the opt-in Rust
engine; see [AUTHENTICATED_ENGINE.md](AUTHENTICATED_ENGINE.md).
