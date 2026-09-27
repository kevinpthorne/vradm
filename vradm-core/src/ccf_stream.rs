//! Bounded phase-bank acquisition of the existing CCF pitch profile, no added
//! wire preamble. Search ten symbol phases on a 40-sample grid for SYNC D391.
//! This is a software/full-duplex profile, not a timing/codec-qualified detector.
use crate::ccf_phy::{decode_pitch, UnverifiedCcf, PITCH_SYMBOL_SAMPLES};

const HOP: usize = 40;
const PHASES: usize = PITCH_SYMBOL_SAMPLES / HOP;
#[derive(Clone, Copy)]
struct Lane {
    sync: u16,
    voiced: usize,
    symbols: usize,
    wire: [u8; 16],
    erased: [bool; 16],
}
impl Lane {
    const fn new() -> Self {
        Self {
            sync: 0,
            voiced: 0,
            symbols: 0,
            wire: [0; 16],
            erased: [false; 16],
        }
    }
    fn symbol(&mut self, value: u8, valid: bool) -> Option<UnverifiedCcf> {
        if self.symbols == 0 {
            self.sync = (self.sync << 4) | value as u16;
            self.voiced = if valid { (self.voiced + 1).min(4) } else { 0 };
            if self.sync == 0xd391 && self.voiced == 4 {
                self.wire = [0; 16];
                self.erased = [false; 16];
                self.wire[0] = 0xd3;
                self.wire[1] = 0x91;
                self.symbols = 4;
            }
        } else {
            let byte = self.symbols / 2;
            if self.symbols % 2 == 0 {
                self.wire[byte] = value << 4;
            } else {
                self.wire[byte] |= value;
            }
            self.erased[byte] |= !valid;
            self.symbols += 1;
            if self.symbols == 32 {
                let result = UnverifiedCcf::from_decisions(self.wire, &self.erased).ok();
                *self = Self::new();
                return result;
            }
        }
        None
    }
}

pub struct CcfStreamProgress {
    pub consumed: usize,
    pub frame: Option<UnverifiedCcf>,
}
pub struct CcfStreamReceiver {
    samples: [i16; 400],
    position: usize,
    filled: usize,
    hop: usize,
    lane: usize,
    lanes: [Lane; PHASES],
    suppress: usize,
}
impl Default for CcfStreamReceiver {
    fn default() -> Self {
        Self::new()
    }
}
impl CcfStreamReceiver {
    pub fn new() -> Self {
        Self {
            samples: [0; 400],
            position: 0,
            filled: 0,
            hop: 0,
            lane: 0,
            lanes: [Lane::new(); PHASES],
            suppress: 0,
        }
    }
    pub fn reset(&mut self) {
        *self = Self::new();
    }
    /// Stops at one channel-valid candidate. Always authenticate it before
    /// using ACKs. Exact physical start/end and EOT completion are not inferred.
    pub fn push(&mut self, input: &[i16]) -> CcfStreamProgress {
        for (index, &sample) in input.iter().enumerate() {
            self.samples[self.position] = sample;
            self.position = (self.position + 1) % 400;
            self.filled = (self.filled + 1).min(400);
            self.hop += 1;
            if self.suppress > 0 {
                self.suppress -= 1;
            }
            if self.hop != HOP {
                continue;
            }
            self.hop = 0;
            let lane = self.lane;
            self.lane = (self.lane + 1) % PHASES;
            if self.filled < 400 || self.suppress > 0 {
                continue;
            }
            let mut symbol = [0; 400];
            let first = 400 - self.position;
            symbol[..first].copy_from_slice(&self.samples[self.position..]);
            symbol[first..].copy_from_slice(&self.samples[..self.position]);
            let decision = decode_pitch(&symbol);
            if let Some(frame) = self.lanes[lane].symbol(decision.symbol, decision.confidence > 0.0)
            {
                // Collapse adjacent phase hypotheses for the same physical CCF.
                self.lanes = [Lane::new(); PHASES];
                self.suppress = 400;
                return CcfStreamProgress {
                    consumed: index + 1,
                    frame: Some(frame),
                };
            }
        }
        CcfStreamProgress {
            consumed: input.len(),
            frame: None,
        }
    }
}
