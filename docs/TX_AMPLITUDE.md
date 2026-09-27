# Transmit RMS ceiling

The live engine now applies `vradm_config_t.tx_amplitude` to PHY burst conditioning.
The effective RMS target is the minimum of that setting and the existing MCS
RMS target. Peak-constrained gain can lower actual RMS below that target. Nominal
samples now remain linear; the specified tanh backstop applies only above 0.45 FS. Zero produces silent bursts; it is an amplitude
setting, not a protocol pause (counters/retry state still advance normally).

`VRADM_CMD_SET_TX_PARAMS` uses `param_f32` as the same ceiling. This is an explicit
project command mapping; the other command parameters currently have no effect.
The host validates the value before queueing, and the audio owner applies it
between buffered bursts. It never rescales a partially played burst. Applied
settings survive link/session reset and new session installation; queued commands
follow the existing command/reset ordering rules. A new engine uses its new config.

The C constructor, authenticated Rust constructor and command entry reject NaN,
infinities and values outside 0–1. The legacy infallible Rust constructor cannot
return an error, so invalid amplitudes fail silent at ceiling zero. Direct PHY
construction retains its prior default; `PhyTransmitter::set_rms_ceiling` is
fallible and leaves the prior setting unchanged on invalid input.

Tests cover live burst RMS/muting/peak limits, C-to-PCM configuration, invalid
inputs without queue consumption, command timing across burst boundaries, reset
persistence and zero callback allocations/frees. This setting applies to data
engine bursts (including their PLCP); separate bootstrap/CCF profile constructors
retain their own documented amplitude behavior. It does not add 16 kHz support,
auto-rate adaptation or codec/limiter qualification.

## Conditioner correction

Normalization now meters energy and computes gain in f64, then converts the
scaled sample to f32. This prevents finite f32 energy overflow and avoids
spurious nonlinear activation from f32 gain rounding at the 0.45 FS boundary.
The output remains signed-i16 and the callback remains allocation-free. Invalid
RMS targets or non-finite addressed input silence the addressed output block;
output beyond the returned count is unchanged. Zero gain still mutes.

The backstop follows the spec's formula only for magnitudes strictly above
0.45 FS. That literal branch has a discontinuity relative to the linear region;
it is retained as specified rather than inventing a new soft-knee curve. Finite
peak-normalized blocks should never reach that branch. Direct tests exercise
exceptional values, including polarity symmetry and the 0.5 FS safety bound.

Conditioner-only tests compare four/eight carrier complex amplitudes with the
ideal linear result and require error below 0.1% (−60 dB). They also bound each
nominal sample's error by PCM quantization. This verifies the compression repair,
not modem-wide TC-10c, vocoder resilience or callback CPU qualification.
