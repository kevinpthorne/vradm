//! Aligned 8 kHz MCS0 CCF payload profile: 32 pitch symbols, 1.6 seconds.
//! No preamble/acquisition or live scheduling is supplied. The caller provides
//! the exact symbol boundary. Channel decoding never authenticates a response;
//! pass UnverifiedCcf into the existing control verifier before state changes.
//! Waveform/nibble conventions are documented in docs/CCF_PCM_PROFILE.md.

use crate::framing::CompactControlFrame;
use std::f32::consts::PI;

pub const PITCH_SYMBOL_SAMPLES: usize = 400;
pub const PITCH_DISCARD_SAMPLES: usize = 120;
pub const CCF_SYMBOLS: usize = 32;
pub const CCF_PCM_SAMPLES: usize = CCF_SYMBOLS * PITCH_SYMBOL_SAMPLES;
const ANALYSIS_SAMPLES: usize = PITCH_SYMBOL_SAMPLES - PITCH_DISCARD_SAMPLES;
const MIN_LAG: usize = 28;
const MAX_LAG: usize = 69;

#[derive(Debug, Clone, Copy)]
pub struct PitchDecision {
    /// Meaningless when confidence == 0; never substitutes a previous symbol.
    pub symbol: u8,
    pub confidence: f32,
}

/// NCCF uses only the final 280 samples; neither side of the lagged product
/// reaches into the discarded first 120. Local-peak interpolation estimates the
/// fractional lag. Earlier near-equal peaks avoid selecting a doubled period.
pub fn decode_pitch(symbol: &[i16; PITCH_SYMBOL_SAMPLES]) -> PitchDecision {
    let mut samples = [0.0f32; ANALYSIS_SAMPLES];
    let mean = symbol[PITCH_DISCARD_SAMPLES..]
        .iter()
        .map(|&v| v as f32)
        .sum::<f32>()
        / ANALYSIS_SAMPLES as f32;
    for (out, &input) in samples.iter_mut().zip(&symbol[PITCH_DISCARD_SAMPLES..]) {
        *out = (input as f32 - mean) / 32768.0;
    }
    let erased = PitchDecision {
        symbol: 0,
        confidence: 0.0,
    };
    if samples.iter().map(|v| v * v).sum::<f32>() < ANALYSIS_SAMPLES as f32 * 1e-8 {
        return erased;
    }
    let mut correlations = [0.0f32; MAX_LAG + 1];
    for lag in MIN_LAG..=MAX_LAG {
        let mut cross = 0.0;
        let mut left = 0.0;
        let mut right = 0.0;
        for n in 0..ANALYSIS_SAMPLES - lag {
            let a = samples[n];
            let b = samples[n + lag];
            cross += a * b;
            left += a * a;
            right += b * b;
        }
        let denominator = (left * right).sqrt();
        if denominator > 1e-12 {
            correlations[lag] = cross / denominator;
        }
    }
    let mut best = 0.0f32;
    let mut lag_choice = None;
    for lag in MIN_LAG + 1..MAX_LAG {
        let score = correlations[lag];
        if score >= 0.30
            && score >= correlations[lag - 1]
            && score > correlations[lag + 1]
            && score > best + 0.01
        {
            best = score;
            lag_choice = Some(lag);
        }
    }
    let Some(lag) = lag_choice else {
        return erased;
    };
    let a = correlations[lag - 1];
    let b = correlations[lag];
    let c = correlations[lag + 1];
    let denominator = a - 2.0 * b + c;
    let adjustment = if denominator.abs() > 1e-9 {
        (0.5 * (a - c) / denominator).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let pitch = 8000.0 / (lag as f32 + adjustment);
    if !(115.0..275.0).contains(&pitch) {
        return erased;
    }
    let index = ((pitch - 120.0) / 10.0).round().clamp(0.0, 15.0) as u8;
    PitchDecision {
        symbol: index,
        confidence: best.clamp(0.0, 1.0),
    }
}

/// Tables are built once, off the audio thread. Rendering performs no heap
/// allocation or trigonometry. High nibble first, then low nibble for each byte.
pub struct CcfPitchTransmitter {
    tones: [[i16; PITCH_SYMBOL_SAMPLES]; 16],
    wire: [u8; 16],
    position: usize,
}
impl CcfPitchTransmitter {
    pub fn new(wire: [u8; 16]) -> Self {
        let tones = core::array::from_fn(|symbol| {
            let mut waveform = [0.0f32; PITCH_SYMBOL_SAMPLES];
            let frequency = 120.0 + symbol as f32 * 10.0;
            for (n, sample) in waveform.iter_mut().enumerate() {
                let phase = 2.0 * PI * frequency * n as f32 / 8000.0;
                *sample = phase.sin();
                let edge = n.min(PITCH_SYMBOL_SAMPLES - 1 - n);
                if edge < 4 {
                    *sample *= 0.5 * (1.0 - (PI * (edge as f32 + 0.5) / 4.0).cos());
                }
            }
            let peak = waveform.iter().map(|v| v.abs()).fold(0.0f32, f32::max);
            waveform.map(|sample| (sample / peak * 0.45 * i16::MAX as f32).round() as i16)
        });
        Self {
            tones,
            wire,
            position: 0,
        }
    }
    pub fn remaining_samples(&self) -> usize {
        CCF_PCM_SAMPLES - self.position
    }
    /// Reuse tables for a new frame, at an explicitly scheduled boundary.
    pub fn reset(&mut self, wire: [u8; 16]) {
        self.wire = wire;
        self.position = 0;
    }
    pub fn render(&mut self, out: &mut [i16]) -> usize {
        let count = out.len().min(self.remaining_samples());
        for sample in &mut out[..count] {
            let symbol = self.position / PITCH_SYMBOL_SAMPLES;
            let byte = self.wire[symbol / 2];
            let value = if symbol % 2 == 0 {
                byte >> 4
            } else {
                byte & 15
            };
            *sample = self.tones[value as usize][self.position % PITCH_SYMBOL_SAMPLES];
            self.position += 1;
        }
        out[count..].fill(0);
        count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcfPcmError {
    TooManyErasures,
    ChannelIntegrity,
}

/// SYNC/RS/CRC-admitted, not MAC-verified. Use codeword() and erasures() with
/// ControlRx::verify_control_response or McsNegotiator::receive. Do not ingest
/// ACKs or switch rate based on channel decoding alone.
#[derive(Debug)]
pub struct UnverifiedCcf {
    wire: [u8; 16],
    erasures: [usize; 8],
    erasure_count: usize,
}
impl UnverifiedCcf {
    pub fn codeword(&self) -> [u8; 16] {
        self.wire
    }
    pub fn erasures(&self) -> &[usize] {
        &self.erasures[..self.erasure_count]
    }
}

pub struct CcfPcmProgress {
    pub consumed: usize,
    pub frame: Option<Result<UnverifiedCcf, CcfPcmError>>,
}

/// Fixed-memory aligned decoder. Both erased nibbles of a byte produce one RS
/// byte erasure. Always consumes all 32 symbols before reporting failure, even
/// when the erasure budget is exceeded; no previous-symbol hold or early abort.
pub struct CcfPitchReceiver {
    symbol: [i16; PITCH_SYMBOL_SAMPLES],
    filled: usize,
    symbols: usize,
    wire: [u8; 16],
    erased: [bool; 16],
}
impl CcfPitchReceiver {
    pub fn at_frame_start() -> Self {
        Self {
            symbol: [0; PITCH_SYMBOL_SAMPLES],
            filled: 0,
            symbols: 0,
            wire: [0; 16],
            erased: [false; 16],
        }
    }
    pub fn reset_to_frame_start(&mut self) {
        *self = Self::at_frame_start();
    }
    pub fn push(&mut self, input: &[i16]) -> CcfPcmProgress {
        let mut consumed = 0;
        while consumed < input.len() && self.symbols < CCF_SYMBOLS {
            let count = (PITCH_SYMBOL_SAMPLES - self.filled).min(input.len() - consumed);
            self.symbol[self.filled..self.filled + count]
                .copy_from_slice(&input[consumed..consumed + count]);
            self.filled += count;
            consumed += count;
            if self.filled == PITCH_SYMBOL_SAMPLES {
                let decision = decode_pitch(&self.symbol);
                let byte = self.symbols / 2;
                self.erased[byte] |= decision.confidence == 0.0;
                if self.symbols % 2 == 0 {
                    self.wire[byte] = decision.symbol << 4;
                } else {
                    self.wire[byte] |= decision.symbol;
                }
                self.symbols += 1;
                self.filled = 0;
                if self.symbols == CCF_SYMBOLS {
                    return CcfPcmProgress {
                        consumed,
                        frame: Some(self.finish()),
                    };
                }
            }
        }
        CcfPcmProgress {
            consumed,
            frame: None,
        }
    }
    fn finish(&self) -> Result<UnverifiedCcf, CcfPcmError> {
        let mut erasures = [0; 8];
        let mut count = 0;
        for (index, &erased) in self.erased.iter().enumerate() {
            if erased {
                if count == 8 {
                    return Err(CcfPcmError::TooManyErasures);
                }
                erasures[count] = index;
                count += 1;
            }
        }
        CompactControlFrame::decode(self.wire, &erasures[..count])
            .map_err(|_| CcfPcmError::ChannelIntegrity)?;
        Ok(UnverifiedCcf {
            wire: self.wire,
            erasures,
            erasure_count: count,
        })
    }
}
