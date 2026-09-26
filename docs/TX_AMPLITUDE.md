# Transmit RMS ceiling

The live engine now applies `vradm_config_t.tx_amplitude` to PHY burst conditioning.
The effective RMS target is the minimum of that setting and the existing MCS
RMS target. Peak-constrained gain and the existing soft limiter still apply, so
the actual RMS may be lower. Zero produces silent bursts; it is an amplitude
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
