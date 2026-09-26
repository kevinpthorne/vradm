//! Opt-in, fixed-memory CCF transmit turn for SPEC §5.1 at 8 kHz.
//! The trusted scheduler supplies a locally signed yielding CCF and permission
//! to transmit. Rendering completion is NOT device playback completion or a
//! channel-ownership grant. See docs/CCF_PCM_PROFILE.md.

use crate::{
    ccf_phy::{CcfPitchTransmitter, CCF_PCM_SAMPLES},
    framing::CompactControlFrame,
};
use std::f32::consts::PI;

pub const EOT_SAMPLES: usize = 1200;
pub const TURN_GUARD_SAMPLES: usize = 1200;
pub const CCF_TURN_SAMPLES: usize = CCF_PCM_SAMPLES + EOT_SAMPLES + TURN_GUARD_SAMPLES;
// Both frequencies repeat after 40 samples at 8 kHz.
const EOT_PERIOD: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcfTurnError {
    Busy,
    InvalidCodeword,
    MissingYield,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcfTurnPhase {
    Idle,
    Ccf,
    EndOfTurn,
    Guard,
}

pub struct CcfTurnTransmitter {
    ccf: CcfPitchTransmitter,
    eot: [i16; EOT_PERIOD],
    position: usize,
}

impl Default for CcfTurnTransmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl CcfTurnTransmitter {
    /// Construct off the audio thread. The EOT profile interprets -12 dBFS as
    /// composite RMS: each sine has peak 10^(-12/20), in phase at sample zero.
    /// Its composite peak remains below 0.5 FS. No trigonometry in render().
    pub fn new() -> Self {
        let amplitude = 10.0f32.powf(-12.0 / 20.0);
        let eot = core::array::from_fn(|n| {
            let phase = 2.0 * PI * n as f32 / 8000.0;
            (((1400.0 * phase).sin() + (1800.0 * phase).sin()) * amplitude * i16::MAX as f32)
                .round() as i16
        });
        Self {
            ccf: CcfPitchTransmitter::new([0; 16]),
            eot,
            position: CCF_TURN_SAMPLES,
        }
    }

    /// Reject replacement of an active turn. Integrity checks catch malformed
    /// local input; they do not authenticate its MAC or authorize transmission.
    /// Require an exact codeword (no implicit correction of local corruption).
    pub fn start(&mut self, wire: [u8; 16]) -> Result<(), CcfTurnError> {
        if self.phase() != CcfTurnPhase::Idle {
            return Err(CcfTurnError::Busy);
        }
        let frame =
            CompactControlFrame::decode(wire, &[]).map_err(|_| CcfTurnError::InvalidCodeword)?;
        if frame.encode() != wire
            || frame.ccf_ctrl & 0x80 == 0
            || (frame.ccf_ctrl >> 4) & 7 > 4
            || !matches!(frame.ccf_ctrl & 7, 1..=3)
            || frame.ack_map & 0x80 != 0
        {
            return Err(CcfTurnError::InvalidCodeword);
        }
        if frame.ccf_ctrl & 8 == 0 {
            return Err(CcfTurnError::MissingYield);
        }
        self.ccf.reset(wire);
        self.position = 0;
        Ok(())
    }

    pub fn phase(&self) -> CcfTurnPhase {
        if self.position < CCF_PCM_SAMPLES {
            CcfTurnPhase::Ccf
        } else if self.position < CCF_PCM_SAMPLES + EOT_SAMPLES {
            CcfTurnPhase::EndOfTurn
        } else if self.position < CCF_TURN_SAMPLES {
            CcfTurnPhase::Guard
        } else {
            CcfTurnPhase::Idle
        }
    }

    pub fn remaining_samples(&self) -> usize {
        CCF_TURN_SAMPLES - self.position
    }

    /// Local abort/rekey only. Does not retract samples already queued to a
    /// device. The device scheduler must invalidate/drain those separately.
    pub fn cancel(&mut self) {
        self.position = CCF_TURN_SAMPLES;
    }

    /// Writes CCF, EOT, then guard without gaps, across arbitrary output chunks.
    /// Returns the count belonging to this turn (including guard silence).
    /// Zero-fills the rest. Idle means all samples were rendered, not played.
    pub fn render(&mut self, out: &mut [i16]) -> usize {
        let count = out.len().min(self.remaining_samples());
        let mut written = 0;
        if self.position < CCF_PCM_SAMPLES {
            let n = count.min(CCF_PCM_SAMPLES - self.position);
            self.ccf.render(&mut out[..n]);
            written += n;
            self.position += n;
        }
        while written < count {
            out[written] = if self.position < CCF_PCM_SAMPLES + EOT_SAMPLES {
                self.eot[(self.position - CCF_PCM_SAMPLES) % EOT_PERIOD]
            } else {
                0
            };
            written += 1;
            self.position += 1;
        }
        out[count..].fill(0);
        count
    }
}

const EOT_WINDOW: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcfTurnReceiveError {
    Ccf(crate::ccf_phy::CcfPcmError),
    MissingEndOfTurn,
}

pub struct CcfTurnProgress {
    pub consumed: usize,
    /// Available only after the complete EOT and guard. Still requires MAC and
    /// transaction verification; acoustic markers cannot authenticate ownership.
    pub frame: Option<Result<crate::ccf_phy::UnverifiedCcf, CcfTurnReceiveError>>,
}

/// Aligned receive component, not an acquisition detector or ownership state
/// machine. All fifteen 10 ms EOT windows must contain both tones. Guard samples
/// advance time but are not required to be silent (room decay is expected).
/// Construct off the audio thread; reset/push perform no allocation or trig.
pub struct CcfTurnReceiver {
    ccf: crate::ccf_phy::CcfPitchReceiver,
    frame: Option<Result<crate::ccf_phy::UnverifiedCcf, crate::ccf_phy::CcfPcmError>>,
    basis: [[[f32; 2]; EOT_WINDOW]; 2],
    window: [f32; EOT_WINDOW],
    filled: usize,
    eot_valid: bool,
    position: usize,
}

impl CcfTurnReceiver {
    pub fn at_frame_start() -> Self {
        let basis = core::array::from_fn(|tone| {
            let frequency = [1400.0, 1800.0][tone];
            core::array::from_fn(|n| {
                let phase = 2.0 * PI * frequency * n as f32 / 8000.0;
                [phase.cos(), phase.sin()]
            })
        });
        Self {
            ccf: crate::ccf_phy::CcfPitchReceiver::at_frame_start(),
            frame: None,
            basis,
            window: [0.0; EOT_WINDOW],
            filled: 0,
            eot_valid: true,
            position: 0,
        }
    }

    /// Required after a capture discontinuity, cancellation or new session.
    /// Caller must then provide a newly established exact CCF sample boundary.
    pub fn reset_to_frame_start(&mut self) {
        self.ccf.reset_to_frame_start();
        self.frame = None;
        self.window.fill(0.0);
        self.filled = 0;
        self.eot_valid = true;
        self.position = 0;
    }

    fn valid_window(&self) -> bool {
        let mean = self.window.iter().sum::<f32>() / EOT_WINDOW as f32;
        let energy = self.window.iter().map(|v| (v - mean).powi(2)).sum::<f32>();
        if energy < EOT_WINDOW as f32 * 1e-6 {
            return false;
        }
        let powers: [f32; 2] = core::array::from_fn(|tone| {
            let mut real = 0.0;
            let mut imaginary = 0.0;
            for (n, &sample) in self.window.iter().enumerate() {
                real += (sample - mean) * self.basis[tone][n][0];
                imaginary += (sample - mean) * self.basis[tone][n][1];
            }
            2.0 * (real * real + imaginary * imaginary) / EOT_WINDOW as f32
        });
        powers[0] >= 0.20 * energy
            && powers[1] >= 0.20 * energy
            && powers[0] + powers[1] >= 0.80 * energy
    }

    /// Always consumes through the guard before reporting, even if CCF or EOT
    /// fails. Stops exactly at the turn boundary and leaves later input untouched.
    /// Completed receivers consume nothing until explicitly reset.
    pub fn push(&mut self, input: &[i16]) -> CcfTurnProgress {
        let count = input.len().min(CCF_TURN_SAMPLES - self.position);
        if self.position == CCF_TURN_SAMPLES {
            return CcfTurnProgress {
                consumed: 0,
                frame: None,
            };
        }
        let mut consumed = 0;
        if self.position < CCF_PCM_SAMPLES {
            let n = count.min(CCF_PCM_SAMPLES - self.position);
            let progress = self.ccf.push(&input[..n]);
            self.position += progress.consumed;
            consumed += progress.consumed;
            if let Some(frame) = progress.frame {
                self.frame = Some(frame);
            }
        }
        while consumed < count {
            if self.position < CCF_PCM_SAMPLES + EOT_SAMPLES {
                self.window[self.filled] = input[consumed] as f32 / 32768.0;
                self.filled += 1;
                if self.filled == EOT_WINDOW {
                    self.eot_valid &= self.valid_window();
                    self.filled = 0;
                }
            }
            self.position += 1;
            consumed += 1;
        }
        let frame = if self.position == CCF_TURN_SAMPLES {
            Some(
                match self.frame.take().expect("complete CCF precedes EOT") {
                    Err(error) => Err(CcfTurnReceiveError::Ccf(error)),
                    Ok(_) if !self.eot_valid => Err(CcfTurnReceiveError::MissingEndOfTurn),
                    Ok(frame) => Ok(frame),
                },
            )
        } else {
            None
        };
        CcfTurnProgress { consumed, frame }
    }
}
