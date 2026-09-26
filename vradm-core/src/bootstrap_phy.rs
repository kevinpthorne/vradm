//! Aligned 8 kHz bootstrap payload codec for SPEC §4.0's 100-baud 2-FSK.
//!
//! Exactly 256 bits / 20,480 samples / 2.56 seconds, without an added preamble.
//! The aligned receiver must be armed at the payload's first sample. The opt-in
//! Barker wrappers below acquire that boundary for the documented project
//! profile. Neither codec recovers sample-clock drift or authenticates frames;
//! pass decoded frames to the host-side session manager before using contents.
//!
//! Bits are sent MSB first, with 1 = 1200 Hz and 0 = 1600 Hz, matching existing
//! PLCP conventions. The optional Barker wrappers below implement the explicit
//! project acquisition profile in docs/BOOTSTRAP_PROFILE.md; they do not change
//! the bare-payload format or enable authentication on the live engine.

use crate::crc::payload_crc16;
use crate::phy::{demodulate_2fsk_bit, FSK_BIT_SAMPLES, FS_8K, V_TARGET_PEAK};
use crate::security::BootstrapWire;
use std::f32::consts::PI;

pub const BOOTSTRAP_BITS: usize = 32 * 8;
pub const BOOTSTRAP_SAMPLES: usize = BOOTSTRAP_BITS * FSK_BIT_SAMPLES;

/// Fixed-memory, arbitrary-chunk PCM renderer. Tone tables are computed once
/// at construction; render performs no allocation or trigonometric operations.
pub struct BootstrapFskTransmitter {
    wire: BootstrapWire,
    position: usize,
    tones: [[i16; FSK_BIT_SAMPLES]; 2],
}

impl BootstrapFskTransmitter {
    pub fn new(wire: BootstrapWire) -> Self {
        let tones = core::array::from_fn(|bit| {
            let hz = if bit == 1 { 1200.0 } else { 1600.0 };
            core::array::from_fn(|n| {
                let phase = 2.0 * PI * hz * n as f32 / FS_8K as f32;
                (phase.cos() * V_TARGET_PEAK * i16::MAX as f32).round() as i16
            })
        });
        // Each bit has exactly 12 or 16 cycles, so the table boundary preserves
        // phase continuity even across a frequency change.
        Self {
            wire,
            position: 0,
            tones,
        }
    }

    pub fn remaining_samples(&self) -> usize {
        BOOTSTRAP_SAMPLES - self.position
    }

    /// Returns payload samples written and zeroes the unused output tail.
    /// Once complete, subsequent calls return zero and render silence.
    pub fn render(&mut self, out: &mut [i16]) -> usize {
        let count = out.len().min(self.remaining_samples());
        for sample in &mut out[..count] {
            let bit_index = self.position / FSK_BIT_SAMPLES;
            let bit = (self.wire[bit_index / 8] >> (7 - bit_index % 8)) & 1;
            *sample = self.tones[bit as usize][self.position % FSK_BIT_SAMPLES];
            self.position += 1;
        }
        out[count..].fill(0);
        count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapDecodeError {
    Erasure,
    Crc,
    Malformed,
}

pub struct BootstrapProgress {
    pub consumed: usize,
    pub frame: Option<Result<BootstrapWire, BootstrapDecodeError>>,
}

/// One aligned payload receiver. Uses one symbol of scratch storage and reports
/// a frame only after all 20,480 samples arrive. Erasures discard the entire
/// payload. Reset explicitly at a newly acquired frame boundary after completion.
pub struct BootstrapFskReceiver {
    samples: [f32; FSK_BIT_SAMPLES],
    sample_count: usize,
    bit_count: usize,
    wire: BootstrapWire,
    erased: bool,
}

impl BootstrapFskReceiver {
    pub fn at_frame_start() -> Self {
        Self {
            samples: [0.0; FSK_BIT_SAMPLES],
            sample_count: 0,
            bit_count: 0,
            wire: [0; 32],
            erased: false,
        }
    }

    pub fn reset_to_frame_start(&mut self) {
        *self = Self::at_frame_start();
    }

    /// Stops precisely at the frame boundary, leaving any following input
    /// unconsumed. Repeated calls after completion produce no duplicate event.
    pub fn push(&mut self, input: &[i16]) -> BootstrapProgress {
        let mut consumed = 0;
        while consumed < input.len() && self.bit_count < BOOTSTRAP_BITS {
            let count = (FSK_BIT_SAMPLES - self.sample_count).min(input.len() - consumed);
            for offset in 0..count {
                self.samples[self.sample_count + offset] =
                    input[consumed + offset] as f32 / i16::MAX as f32;
            }
            consumed += count;
            self.sample_count += count;
            if self.sample_count == FSK_BIT_SAMPLES {
                let (bit, confidence) = demodulate_2fsk_bit(&self.samples);
                let energy: f32 = self.samples.iter().map(|v| v * v).sum();
                // Prototype erasure thresholds, not codec-qualified gates.
                self.erased |= confidence < 0.5 || energy < FSK_BIT_SAMPLES as f32 * 1e-6;
                if bit {
                    self.wire[self.bit_count / 8] |= 1 << (7 - self.bit_count % 8);
                }
                self.bit_count += 1;
                self.sample_count = 0;
                if self.bit_count == BOOTSTRAP_BITS {
                    return BootstrapProgress {
                        consumed,
                        frame: Some(self.finish()),
                    };
                }
            }
        }
        BootstrapProgress {
            consumed,
            frame: None,
        }
    }

    fn finish(&self) -> Result<BootstrapWire, BootstrapDecodeError> {
        if self.erased {
            return Err(BootstrapDecodeError::Erasure);
        }
        let crc = u16::from_be_bytes([self.wire[29], self.wire[30]]);
        if payload_crc16(&self.wire[..29]) != crc {
            return Err(BootstrapDecodeError::Crc);
        }
        if !matches!(self.wire[0], 0xbe | 0xbf)
            || self.wire[31] != 0
            || (self.wire[0] == 0xbe && self.wire[17..21] != [0; 4])
        {
            return Err(BootstrapDecodeError::Malformed);
        }
        Ok(self.wire)
    }
}

/// Opt-in project profile: Barker-13 (65 ms), then the unchanged FSK payload.
/// No guard samples are included; the scheduler owns turnaround guard timing.
/// This profile is documented in docs/BOOTSTRAP_PROFILE.md, not yet negotiated
/// or enabled by the engine's C ABI.
pub const BARKER_BOOTSTRAP_SAMPLES: usize = crate::phy::BARKER_TOTAL_SAMPLES + BOOTSTRAP_SAMPLES;

pub struct BarkerBootstrapTransmitter {
    preamble: [i16; crate::phy::BARKER_TOTAL_SAMPLES],
    preamble_position: usize,
    payload: BootstrapFskTransmitter,
}

impl BarkerBootstrapTransmitter {
    pub fn new(wire: BootstrapWire) -> Self {
        Self {
            preamble: crate::phy::synthesize_barker_preamble()
                .map(|sample| (sample * V_TARGET_PEAK * i16::MAX as f32).round() as i16),
            preamble_position: 0,
            payload: BootstrapFskTransmitter::new(wire),
        }
    }

    pub fn remaining_samples(&self) -> usize {
        self.preamble.len() - self.preamble_position + self.payload.remaining_samples()
    }

    pub fn render(&mut self, out: &mut [i16]) -> usize {
        let count = out.len().min(self.preamble.len() - self.preamble_position);
        out[..count].copy_from_slice(
            &self.preamble[self.preamble_position..self.preamble_position + count],
        );
        self.preamble_position += count;
        count + self.payload.render(&mut out[count..])
    }
}

struct AcquisitionPeak {
    magnitude: f32,
    after: [i16; 4],
    after_len: usize,
}

/// Streaming receiver for the opt-in Barker bootstrap profile. Searches on every
/// sample using fixed memory, then verifies the complete payload through the
/// aligned decoder. An event ends the current push; callers must resubmit its
/// unconsumed suffix. Searching resumes automatically after success or failure.
/// It never authenticates a frame or updates a session/ARQ context itself.
pub struct BarkerBootstrapReceiver {
    template: [f32; crate::phy::BARKER_TOTAL_SAMPLES],
    template_energy: f32,
    window: [f32; crate::phy::BARKER_TOTAL_SAMPLES],
    window_next: usize,
    window_filled: usize,
    peak: Option<AcquisitionPeak>,
    receiving: bool,
    payload: BootstrapFskReceiver,
}

impl BarkerBootstrapReceiver {
    pub fn new() -> Self {
        let template = crate::phy::synthesize_barker_preamble();
        let template_energy = template.iter().map(|v| v * v).sum();
        Self {
            template,
            template_energy,
            window: [0.0; crate::phy::BARKER_TOTAL_SAMPLES],
            window_next: 0,
            window_filled: 0,
            peak: None,
            receiving: false,
            payload: BootstrapFskReceiver::at_frame_start(),
        }
    }

    /// Drop partial acquisition/payload on a trusted stream discontinuity.
    pub fn reset(&mut self) {
        self.window_next = 0;
        self.window_filled = 0;
        self.peak = None;
        self.receiving = false;
        self.payload.reset_to_frame_start();
    }

    fn correlation(&self) -> f32 {
        let mut cross = 0.0;
        let mut energy = 0.0;
        // window_next points to the oldest sample once the ring is full.
        let ordered = self.window[self.window_next..]
            .iter()
            .chain(self.window[..self.window_next].iter());
        for (&sample, &reference) in ordered.zip(self.template.iter()) {
            cross += sample * reference;
            energy += sample * sample;
        }
        if energy < self.window.len() as f32 * 1e-6 {
            return 0.0;
        }
        (cross / (energy * self.template_energy).sqrt()).abs()
    }

    pub fn push(&mut self, input: &[i16]) -> BootstrapProgress {
        let mut consumed = 0;
        while consumed < input.len() {
            if self.receiving {
                let progress = self.payload.push(&input[consumed..]);
                consumed += progress.consumed;
                if let Some(frame) = progress.frame {
                    self.reset();
                    return BootstrapProgress {
                        consumed,
                        frame: Some(frame),
                    };
                }
                break;
            }
            let sample = input[consumed];
            consumed += 1;
            self.window[self.window_next] = sample as f32 / i16::MAX as f32;
            self.window_next = (self.window_next + 1) % self.window.len();
            self.window_filled = (self.window_filled + 1).min(self.window.len());
            if self.window_filled < self.window.len() {
                continue;
            }
            let magnitude = self.correlation();
            // High absolute threshold rejects Barker's ~0.75 polarity-reversed
            // sidelobe while allowing a genuinely inverted preamble. Four samples
            // of lookahead choose the local peak independent of callback size.
            if magnitude >= 0.85
                && self
                    .peak
                    .as_ref()
                    .map_or(true, |peak| magnitude > peak.magnitude)
            {
                self.peak = Some(AcquisitionPeak {
                    magnitude,
                    after: [0; 4],
                    after_len: 0,
                });
            } else if let Some(peak) = &mut self.peak {
                peak.after[peak.after_len] = sample;
                peak.after_len += 1;
                if peak.after_len == peak.after.len() {
                    // These lookahead samples belong to the payload; do not lose
                    // them when transitioning from acquisition to demodulation.
                    self.payload.reset_to_frame_start();
                    self.payload.push(&peak.after);
                    self.receiving = true;
                    self.peak = None;
                }
            }
        }
        BootstrapProgress {
            consumed,
            frame: None,
        }
    }
}
