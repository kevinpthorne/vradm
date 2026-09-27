# Aligned CCF pitch PCM profile

`ccf_phy` implements an opt-in 8 kHz signed-i16 transport component for the
16-byte Compact Control Frame. It is not connected to the live engine. Callers
must supply the exact first sample of the first symbol; there is no acquisition,
preamble, timing tracking, TDD ownership or device playback scheduling here.

## Waveform and mapping

SPEC §2.2 and §3.2 supply 32 four-bit symbols at 20 baud, with pitch
`120 + 10 * symbol` Hz. Each symbol occupies 400 samples; a CCF occupies 12,800
samples (1.6 seconds). Bytes remain in codeword order, high nibble then low nibble.
The latter convention and exact waveform boundaries are explicit project profile
choices, not an assertion of interoperability with another implementation.

Consistent with the spec's single-tone MCS0 amplitude table, each symbol is a
sine starting at phase zero. The first/last four samples use a raised-cosine
edge taper: `0.5 * (1 - cos(pi * (edge + 0.5) / 4))`, where edge is distance
from the nearest endpoint. Each tone is normalized to peak 0.45 full scale;
its RMS is below the 0.3535 ceiling because peak headroom is the limiting factor.
Quantization rounds against i16::MAX. Tables are built in the transmitter
constructor off the audio thread; render/reset reuse them without allocation
or trigonometry. Rendering zero-fills any output after the frame and returns
only the number of frame samples written.

## Receive and authentication boundary

The decoder discards the first 120 samples of every symbol and uses only the
last 280 samples for both sides of its normalized autocorrelation. It removes
DC, searches integer lags 28–69, interpolates local peaks, and maps estimated
pitch to the nearest alphabet entry. The accepted peak must be at least 0.30;
very low energy or absent valid periodic peaks produces zero-confidence erasure.
An earlier near-equal peak is preferred over a doubled period. These are bounded
heuristics, not a qualified noise/codec classifier or clock recovery loop.

Each erased nibble marks its containing byte erased. Erasing both halves still
counts once. RS(16,8) permits up to eight erased bytes, or mixed errors/erasures
within `2 * errors + erasures <= 8`. The receiver always consumes all 32 symbols
before reporting success or failure, including when the budget is already
exceeded. It never substitutes a previous pitch. A completed receiver consumes
no more samples until explicitly reset at a new aligned boundary.

`push` accepts arbitrary chunks and reports exact consumption at frame end.
SYNC/RS/CRC success produces `UnverifiedCcf`; it does **not** authorize ACKs or
control effects. Pass its raw `codeword()` and `erasures()` to
`ControlRx::verify_control_response` or `McsNegotiator::receive`. The existing
verifier repeats bounded RS decoding and applies MAC, counter, deadline and
response-semantic checks. Keeping the raw codeword plus erasures preserves that
single authenticated admission path. A valid CRC with forged MAC remains rejected.

## Validation and missing work

Tests cover all sixteen pitches, gain/polarity, DC and deterministic noise,
first-15ms exclusion, chunking and frame boundaries, eight byte erasures,
mixed errors/erasures, full-frame failure timing, forged MAC rejection, deferred
MCS commit, transmitter reset and zero heap allocations/frees during render,
receive and RS recovery. They establish software loopback behavior only.

Still needed: acquisition and symbol/clock alignment, confidence-ranked retries,
CCF turn scheduling and guards, receiver metric admission, live ARQ/negotiation
integration, device adapters, and speech-codec/channel qualification. Single tones
at 120–270 Hz are particularly dependent on the actual voice path's filtering;
software success does not establish that they survive a phone or PBX call.
This component does not repair the live engine's MCS0 data-frame fallback.

## Bounded CCF transmit turn

`ccf_turn::CcfTurnTransmitter` adds a separate SPEC §5.1 transmit component:
12,800 CCF samples, immediately followed by 1,200 EOT samples and 1,200 silent
guard samples. Total rendered duration is 15,200 samples (1.9 seconds). No PLCP
is added. Output chunks may cross any boundary; the return count includes guard
silence and excludes zero padding after completion. Phase reports the next
sample's phase, becoming Idle only after the last guard sample is rendered.

The EOT profile uses phase-zero 1400 Hz and 1800 Hz sines and interprets the
spec's -12 dBFS as **composite RMS**. Each sine's peak is `10^(-12/20)` full scale;
their orthogonal sum has that RMS and a peak below 0.5 FS. A 40-sample table
repeats exactly 30 times. This explicit amplitude/phase convention resolves
otherwise unspecified details; codec/AEC survival is not established. Construction
builds tables off the audio thread; start/render/cancel allocate and free nothing.

A trusted scheduler must sign the frame for the verified peer's full counter
and establish permission to transmit before calling `start`. The renderer checks
an exact RS/CRC codeword, supported structural fields and the yield bit; it does
not verify the MAC or grant channel ownership. A locally damaged codeword is
rejected even if RS could correct it. Starting while active returns Busy without
changing the current turn, including during guard. Explicit local `cancel`
silences future rendering and allows reuse; it cannot retract queued device audio.

**Rendered completion is not playback completion.** Before changing physical
ownership, the eventual device scheduler must acknowledge actual playback/drain
through its own generation-bound mechanism. This component provides no drain
token, turn timeout, collision recovery, input acquisition
or live engine routing. It must not be used as a substitute for those controls.
Tests cover sample-exact phases, arbitrary chunks, no replacement during guard,
EOT spectral/RMS/peak bounds, malformed local frames, cancel/restart and allocation.

## Aligned receive turn and EOT admission

`ccf_turn::CcfTurnReceiver` starts at a caller-established exact CCF boundary,
decodes the CCF and validates the following EOT. It emits its result only after
all 15,200 turn samples, including the full 150 ms guard, have been consumed.
Arbitrary chunks stop exactly at that boundary; subsequent input is untouched.
A completed receiver stays stopped until `reset_to_frame_start`. Capture gaps,
rekey or cancellation require resetting and establishing a fresh boundary;
missing audio must never be treated as elapsed guard samples implicitly.

EOT admission uses fifteen consecutive non-overlapping 80-sample (10 ms) windows.
After DC removal, each window must have RMS at least 0.001 full scale, each of
1400 Hz and 1800 Hz must account for at least 20% of its energy, and their sum
must account for at least 80%. Sine/cosine projection allows arbitrary tone
phase and polarity. Tables initialize off the audio thread; push/reset do no
allocation or trigonometry. Thresholds are explicit conservative project-profile
choices, not spec-defined or device-qualified detector thresholds. All fifteen
windows must pass; no dropout tolerance or unknown-boundary search is supplied.

Guard samples advance the receive sample count; they need not be silent because
room decay is expected. Silence, DC, noise, a single tone, wrong-frequency tones,
or a missing EOT window fail admission. CCF or EOT failure is reported after the
full guard, with CCF errors taking precedence. This keeps chunk consumption
predictable even on corrupted input. Reset discards any stored CCF/EOT evidence.

Success still returns **UnverifiedCcf**. The caller must use the existing MAC,
counter, deadline and transaction-semantic verifier before applying ACKs or turn
ownership. EOT tones themselves are unauthenticated and cannot grant a token.
The aligned receiver adds no live engine routing, playback acknowledgment,
collision recovery or acquisition of an unknown CCF boundary. Timing/codec/drift
qualification and receive schedulers remain future work.

## Live full-duplex integration

[LIVE_CCF.md](LIVE_CCF.md) describes the new phase-bank acquisition and live engine
standalone-ACK path. It does not use aligned EOT/guard completion as an acoustic
ownership signal; the aligned components above keep their original contracts.
