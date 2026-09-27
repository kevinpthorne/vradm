use crate::security::Beacon;
use std::f32::consts::PI;
use crate::framing::CanonicalDataFrame;
use crate::fec::{golay_encode, golay_decode};

pub const FS_8K: usize = 8000;
pub const AUDIO_CHUNK_20MS: usize = 160;
pub const AUDIO_CHUNK_40MS: usize = 320;
pub const AUDIO_RING_CAPACITY: usize = 65536; // 8.192 s @ 8 kHz

pub const BARKER_CHIPS: usize = 13;
pub const BARKER_CHIP_SAMPLES: usize = 40;
pub const BARKER_TOTAL_SAMPLES: usize = 520; // 65.0 ms
pub const FSK_BIT_SAMPLES: usize = 80;       // 10.0 ms @ 100 Bd
pub const FSK_TOTAL_BITS: usize = 48;
pub const FSK_TOTAL_SAMPLES: usize = 3840;   // 480.0 ms

pub const PLCP_NOMINAL_GUARD_INTER: usize = 80;  // 10.0 ms
pub const PLCP_NOMINAL_GUARD_POST: usize = 80;   // 10.0 ms
pub const PLCP_NOMINAL_TOTAL: usize = 4520;      // 565.0 ms

pub const PLCP_MCS3_GUARD_INTER: usize = 0;      // 0.0 ms (SPEC §4.2)
pub const PLCP_MCS3_GUARD_POST: usize = 280;     // 35.0 ms (SPEC §4.2)
pub const PLCP_MCS3_TOTAL: usize = 4640;         // 580.0 ms (116 * 40 = 29 * 160)

pub const MAX_BURST_SAMPLES: usize = 46120;      // 4520 + 8 * 5200
pub const RX_SAMPLE_CAPACITY: usize = 65536;

pub const V_PEAK_MAX: f32 = 0.50;
pub const V_TARGET_PEAK: f32 = 0.45;
pub const RMS_TARGET_MCS: [f32; 5] = [0.3535, 0.2239, 0.1778, 0.1334, 0.1585];

// ============================================================================
// SipHash-2-4 (Self-Contained Pure Rust)
// ============================================================================

#[inline(always)]
fn sip_round(v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64) {
    *v0 = v0.wrapping_add(*v1);
    *v1 = v1.rotate_left(13);
    *v1 ^= *v0;
    *v0 = v0.rotate_left(32);

    *v2 = v2.wrapping_add(*v3);
    *v3 = v3.rotate_left(16);
    *v3 ^= *v2;

    *v0 = v0.wrapping_add(*v3);
    *v3 = v3.rotate_left(21);
    *v3 ^= *v0;

    *v2 = v2.wrapping_add(*v1);
    *v1 = v1.rotate_left(17);
    *v1 ^= *v2;
    *v2 = v2.rotate_left(32);
}

pub fn siphash_2_4(key: &[u8; 16], data: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[0..8].try_into().unwrap());
    let k1 = u64::from_le_bytes(key[8..16].try_into().unwrap());

    let mut v0 = k0 ^ 0x736f6d6570736575;
    let mut v1 = k1 ^ 0x646f72616e646f6d;
    let mut v2 = k0 ^ 0x6c7967656e657261;
    let mut v3 = k1 ^ 0x7465646279746573;

    let len = data.len();
    let num_words = len / 8;
    for i in 0..num_words {
        let m = u64::from_le_bytes(data[i * 8..(i + 1) * 8].try_into().unwrap());
        v3 ^= m;
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= m;
    }

    let remainder = &data[num_words * 8..];
    let mut last_word = ((len as u64) & 0xFF) << 56;
    for (i, &b) in remainder.iter().enumerate() {
        last_word |= (b as u64) << (i * 8);
    }

    v3 ^= last_word;
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= last_word;

    v2 ^= 0xFF;
    for _ in 0..4 {
        sip_round(&mut v0, &mut v1, &mut v2, &mut v3);
    }

    v0 ^ v1 ^ v2 ^ v3
}

// ============================================================================
// PRBS-7 Phase Dither Generator ($x^7 + x^6 + 1$)
// ============================================================================

#[derive(Debug, Clone)]
pub struct Prbs7Dither {
    reg: u8,
    raw_prev: f32,
    raw_curr: f32,
}

impl Prbs7Dither {
    pub fn new(beac_seq: u8, dir: u8, frame_index: u8) -> Self {
        let seed = ((beac_seq as u16 ^ ((dir as u16) << 7) ^ (frame_index as u16 * 17)) % 127) as u8 + 1;
        let mut dither = Self {
            reg: seed,
            raw_prev: 0.0,
            raw_curr: 0.0,
        };
        dither.step();
        dither.raw_prev = dither.raw_curr;
        dither
    }

    pub fn step(&mut self) -> f32 {
        self.raw_prev = self.raw_curr;
        let fb = ((self.reg >> 6) ^ (self.reg >> 5)) & 1;
        self.reg = ((self.reg << 1) & 0x7F) | fb;
        let norm = (self.reg as f32) / 127.0; // (0.0..1.0]
        self.raw_curr = (PI / 16.0) * (2.0 * norm - 1.0); // [-pi/16, +pi/16]
        self.raw_curr
    }

    pub fn current(&self) -> f32 {
        self.raw_curr
    }

    pub fn get_sample_dither(&self, n_in_sym: usize, edge_len: usize) -> f32 {
        if n_in_sym < edge_len {
            let factor = 0.5 * (1.0 - (PI * (n_in_sym as f32 + 0.5) / edge_len as f32).cos());
            self.raw_prev + (self.raw_curr - self.raw_prev) * factor
        } else {
            self.raw_curr
        }
    }
}

// ============================================================================
// Barker-13 Dual-Chirp & 2-FSK PLCP Waveform Synthesis
// ============================================================================

pub fn synthesize_barker_preamble() -> [f32; 520] {
    let barker = [1.0f32, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 1.0, 1.0, -1.0, 1.0, -1.0, 1.0];
    let mut preamble = [0.0f32; 520];
    let fs = 8000.0f32;
    let f1 = 600.0f32;
    let f2 = 1800.0f32;
    let t_sub = 0.0025f32;

    for k in 0..13 {
        let b = barker[k];
        for m in 0..40 {
            let taper = if m == 0 || m == 39 {
                0.5 * (1.0 - (PI * 0.5 / 2.0).cos())
            } else if m == 1 || m == 38 {
                0.5 * (1.0 - (PI * 1.5 / 2.0).cos())
            } else {
                1.0
            };

            let chirp = if m < 20 {
                let t = m as f32 / fs;
                let phi = 2.0 * PI * (f1 * t + (f2 - f1) / (2.0 * t_sub) * t * t);
                phi.cos()
            } else {
                let t = (m - 20) as f32 / fs;
                let phi_down_init = 6.0 * PI;
                let phi = phi_down_init + 2.0 * PI * (f2 * t - (f2 - f1) / (2.0 * t_sub) * t * t);
                phi.cos()
            };

            preamble[k * 40 + m] = b * taper * chirp;
        }
    }
    preamble
}

pub fn encode_plcp_header_bits(
    cur_mcs: u8,
    req_mcs: u8,
    tx_pwr: u8,
    beac_seq: u8,
    beacon_mac8: u8,
) -> [bool; 48] {
    let m1 = ((cur_mcs as u16 & 0x07) << 9)
        | ((req_mcs as u16 & 0x07) << 6)
        | ((tx_pwr as u16 & 0x03) << 4)
        | (((beac_seq as u16) >> 4) & 0x0F);
    let c1 = golay_encode(m1);

    let m2 = (((beac_seq as u16) & 0x0F) << 8) | (beacon_mac8 as u16);
    let c2 = golay_encode(m2);

    let mut bits = [false; 48];
    for i in 0..24 {
        bits[i] = ((c1 >> (23 - i)) & 1) != 0;
    }
    for i in 0..24 {
        bits[24 + i] = ((c2 >> (23 - i)) & 1) != 0;
    }
    bits
}

pub fn synthesize_2fsk_header(bits: &[bool; 48]) -> [f32; 3840] {
    let mut out = [0.0f32; 3840];
    let fs = 8000.0f32;
    let mut phase = 0.0f32;

    for (bit_idx, &bit) in bits.iter().enumerate() {
        let freq = if bit { 1200.0f32 } else { 1600.0f32 };
        let dphi = 2.0 * PI * freq / fs;
        for n in 0..80 {
            out[bit_idx * 80 + n] = phase.cos();
            phase += dphi;
            if phase >= 2.0 * PI {
                phase -= 2.0 * PI;
            }
        }
    }
    out
}

pub fn decode_plcp_header(bits: &[bool; 48]) -> Result<(u8, u8, u8, u8, u8), &'static str> {
    let mut c1 = 0u32;
    for i in 0..24 {
        if bits[i] {
            c1 |= 1 << (23 - i);
        }
    }
    let mut c2 = 0u32;
    for i in 0..24 {
        if bits[24 + i] {
            c2 |= 1 << (23 - i);
        }
    }

    let m1 = golay_decode(c1).map_err(|_| "Golay decode failure on Codeword 1")?;
    let m2 = golay_decode(c2).map_err(|_| "Golay decode failure on Codeword 2")?;

    let cur_mcs = ((m1 >> 9) & 0x07) as u8;
    let req_mcs = ((m1 >> 6) & 0x07) as u8;
    let tx_pwr = ((m1 >> 4) & 0x03) as u8;
    let beac_seq = ((((m1 & 0x0F) << 4) | ((m2 >> 8) & 0x0F)) & 0xFF) as u8;
    let beacon_mac8 = (m2 & 0xFF) as u8;

    Ok((cur_mcs, req_mcs, tx_pwr, beac_seq, beacon_mac8))
}

// ============================================================================
// Output Normalizer & Hyperbolic Tangent Soft Limiter
// ============================================================================

pub fn condition_and_quantize_pcm_into(samples: &[f32], target_rms: f32, out_pcm: &mut [i16]) -> usize {
    let n = samples.len().min(out_pcm.len());
    if n == 0 {
        return 0;
    }

    // A bad DSP sample must not poison the gain or reach the device. Reject
    // the whole addressed block, preserving the caller-owned output suffix.
    if !target_rms.is_finite() || target_rms < 0.0
        || samples[..n].iter().any(|s| !s.is_finite()) {
        out_pcm[..n].fill(0);
        return n;
    }

    // f64 metering avoids f32 energy overflow and prevents gain-rounding noise
    // at the 0.45 FS boundary from spuriously activating the nonlinear branch.
    // All finite f32 input magnitudes can be squared safely in this accumulator.
    let mut sum_sq = 0.0f64;
    let mut max_abs = 0.0f64;
    for &sample in &samples[..n] {
        let sample = sample as f64;
        sum_sq += sample * sample;
        max_abs = max_abs.max(sample.abs());
    }
    let current_rms = (sum_sq / n as f64).sqrt().max(1e-6);
    let g_rms = target_rms as f64 / current_rms;
    let g_peak = V_TARGET_PEAK as f64 / max_abs.max(1e-6);
    let gain = g_rms.min(g_peak);

    for i in 0..n {
        let scaled = (samples[i] as f64 * gain) as f32;
        let limited = limit_exceptional_peak(scaled);
        out_pcm[i] = (limited * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
    }
    n
}

// SPEC §3 scopes tanh to exceptional excursions, not every nominal sample.
// With finite peak-normalized blocks this branch should never be needed;
// retaining it protects future post-normalization transient-producing stages.
fn limit_exceptional_peak(sample: f32) -> f32 {
    if sample.abs() > V_TARGET_PEAK {
        V_PEAK_MAX * (sample / V_PEAK_MAX).tanh()
    } else {
        sample
    }
}

#[cfg(test)]
mod limiter_backstop_tests {
    use super::*;
    #[test]
    fn limiter_is_identity_through_threshold_and_bounds_exceptional_peaks() {
        for sample in [-V_TARGET_PEAK, -0.1, -0.0, 0.0, 0.1, V_TARGET_PEAK] {
            assert_eq!(limit_exceptional_peak(sample).to_bits(), sample.to_bits());
        }
        for sample in [0.451, 0.5, 1.0, 10.0, f32::MAX] {
            let output = limit_exceptional_peak(sample);
            assert_eq!(output, 0.5 * (sample / 0.5).tanh());
            assert!(output > 0.0 && output <= V_PEAK_MAX);
            assert_eq!(limit_exceptional_peak(-sample), -output);
        }
    }
}

pub fn condition_and_quantize_pcm(samples: &[f32], target_rms: f32) -> Vec<i16> {
    let mut pcm = vec![0i16; samples.len()];
    let n = condition_and_quantize_pcm_into(samples, target_rms, &mut pcm);
    pcm.truncate(n);
    pcm
}

// ============================================================================
// Barker-13 Matched-Filter Correlation Detector
// ============================================================================

pub struct BarkerDetector {
    template: [f32; 520],
    template_energy: f32,
}

impl BarkerDetector {
    pub fn new() -> Self {
        let template = synthesize_barker_preamble();
        let mut e = 0.0f32;
        for &s in &template {
            e += s * s;
        }
        Self {
            template,
            template_energy: e,
        }
    }

    pub fn correlate(&self, buffer: &[f32]) -> (f32, f32) {
        if buffer.len() < 520 {
            return (0.0, 0.0);
        }
        let mut cross = 0.0f32;
        let mut sig_e = 0.0f32;
        for i in 0..520 {
            cross += buffer[i] * self.template[i];
            sig_e += buffer[i] * buffer[i];
        }
        let denom = (sig_e * self.template_energy).sqrt().max(1e-6);
        (cross / denom, sig_e)
    }
}

// ============================================================================
// 2-FSK Quadrature Energy Demodulator
// ============================================================================

pub fn demodulate_2fsk_bit(samples: &[f32; 80]) -> (bool, f32) {
    let fs = 8000.0f32;
    let mut i_mark = 0.0f32;
    let mut q_mark = 0.0f32;
    let mut i_space = 0.0f32;
    let mut q_space = 0.0f32;

    for n in 0..80 {
        let t = n as f32 / fs;
        let phi_m = 2.0 * PI * 1200.0 * t;
        let phi_s = 2.0 * PI * 1600.0 * t;
        let s = samples[n];
        i_mark += s * phi_m.cos();
        q_mark += s * phi_m.sin();
        i_space += s * phi_s.cos();
        q_space += s * phi_s.sin();
    }

    let e_mark = i_mark * i_mark + q_mark * q_mark;
    let e_space = i_space * i_space + q_space * q_space;
    let bit = e_mark > e_space;
    let confidence = ((e_mark - e_space).abs() / (e_mark + e_space).max(1e-6)).min(1.0);
    (bit, confidence)
}

// ============================================================================
// Multi-Carrier DQPSK Modulator & Demodulator (MCS 2 & MCS 3)
// ============================================================================

pub struct DqpskModulator {
    pub mcs: u8,
    pub num_carriers: usize,
    pub carriers: [f32; 8],
    pub amplitudes: [f32; 8],
    pub current_phases: [f32; 8],
}

impl DqpskModulator {
    pub fn new(mcs: u8) -> Self {
        let mut m = Self {
            mcs: 0,
            num_carriers: 0,
            carriers: [0.0; 8],
            amplitudes: [0.0; 8],
            current_phases: [0.0; 8],
        };
        m.reset_for_mcs(mcs);
        m
    }

    pub fn reset_for_mcs(&mut self, mcs: u8) {
        self.mcs = mcs;
        match mcs {
            2 => {
                self.num_carriers = 4;
                self.carriers[0..4].copy_from_slice(&[600.0, 1000.0, 1400.0, 1800.0]);
                self.amplitudes[0..4].copy_from_slice(&[0.8, 1.0, 0.9, 0.7]);
                for k in 0..4 {
                    self.current_phases[k] = k as f32 * PI / 4.0;
                }
            }
            3 => {
                self.num_carriers = 8;
                for k in 0..8 {
                    self.carriers[k] = (k as f32 + 3.0) * 200.0;
                }
                self.amplitudes[0..8].copy_from_slice(&[0.6, 0.9, 1.0, 0.85, 0.7, 0.5, 0.4, 0.3]);
                for k in 0..8 {
                    self.current_phases[k] = k as f32 * PI / 4.0;
                }
            }
            4 => {
                self.num_carriers = 8;
                // SPEC §3.2 Telephone-Band Carrier Allocation (Z = 8 Active Carriers):
                // k in {2, 3, 5, 6, 7, 8, 9, 10}, Delta f = 8000 / 28 Hz
                // f_k in {571.43, 857.14, 1428.57, 1714.29, 2000.00, 2285.71, 2571.43, 2857.14} Hz
                const CARRIERS_MCS4: [f32; 8] = [
                    571.4286,  // k = 2
                    857.1429,  // k = 3
                    1428.5714, // k = 5
                    1714.2857, // k = 6
                    2000.0,    // k = 7
                    2285.7144, // k = 8
                    2571.4287, // k = 9
                    2857.1428, // k = 10
                ];
                self.carriers[0..8].copy_from_slice(&CARRIERS_MCS4);
                // Unit-energy constellation per SPEC §3.2, Table 3.3 Target RMS = 0.1585 FS
                self.amplitudes[0..8].copy_from_slice(&[1.0; 8]);
                for k in 0..8 {
                    self.current_phases[k] = k as f32 * PI / 4.0;
                }
            }
            _ => {
                self.num_carriers = 4;
                self.carriers[0..4].copy_from_slice(&[600.0, 1000.0, 1400.0, 1800.0]);
                self.amplitudes[0..4].copy_from_slice(&[0.8, 1.0, 0.9, 0.7]);
                for k in 0..4 {
                    self.current_phases[k] = k as f32 * PI / 4.0;
                }
            }
        }
    }

    pub fn modulate_frame_into(
        &mut self,
        interleaved: &[u8; 64],
        dither: &mut Prbs7Dither,
        out: &mut [f32],
    ) {
        let (n_sym, total_symbols) = if self.mcs == 2 { (80, 65) } else { (40, 33) };
        let frame_samples = total_symbols * n_sym;
        assert!(out.len() >= frame_samples, "out buffer too small");

        let fs = 8000.0f32;
        let z = self.num_carriers;
        let mut out_idx = 0;

        // Symbol 0: Reference symbol
        for n in 0..n_sym {
            let mut s = 0.0f32;
            let d = dither.get_sample_dither(n, 4);
            for k in 0..z {
                let phi = 2.0 * PI * self.carriers[k] * (n as f32 / fs) + self.current_phases[k] + d;
                s += self.amplitudes[k] * phi.cos();
            }
            let w = raised_cosine_window(n, n_sym, 4);
            out[out_idx] = s * w;
            out_idx += 1;
        }

        // Data symbols
        let mut bit_offset = 0;
        let total_bits = 64 * 8; // 512 bits

        while bit_offset < total_bits {
            dither.step();
            // Advance phase per carrier based on Gray-coded dibits
            for k in 0..z {
                let bit1 = (interleaved[bit_offset / 8] >> (7 - (bit_offset % 8))) & 1;
                let bit0 = (interleaved[(bit_offset + 1) / 8] >> (7 - ((bit_offset + 1) % 8))) & 1;
                bit_offset += 2;

                let dphi = match (bit1, bit0) {
                    (0, 0) => 0.0,
                    (0, 1) => PI / 2.0,
                    (1, 1) => PI,
                    (1, 0) => -PI / 2.0,
                    _ => 0.0,
                };
                self.current_phases[k] = wrap_2pi(self.current_phases[k] + dphi);
            }

            for n in 0..n_sym {
                let mut s = 0.0f32;
                let d = dither.get_sample_dither(n, 4);
                for k in 0..z {
                    let phi = 2.0 * PI * self.carriers[k] * (n as f32 / fs) + self.current_phases[k] + d;
                    s += self.amplitudes[k] * phi.cos();
                }
                let w = raised_cosine_window(n, n_sym, 4);
                out[out_idx] = s * w;
                out_idx += 1;
            }
        }
    }

    pub fn modulate_frame(&mut self, interleaved: &[u8; 64], dither: &mut Prbs7Dither) -> Vec<f32> {
        let (n_sym, total_symbols) = if self.mcs == 2 { (80, 65) } else { (40, 33) };
        let mut out = vec![0.0f32; total_symbols * n_sym];
        self.modulate_frame_into(interleaved, dither, &mut out);
        out
    }
}

pub struct DqpskDemodulator {
    pub mcs: u8,
    pub num_carriers: usize,
    pub carriers: [f32; 8],
    pub prev_phases: [f32; 8],
}

impl DqpskDemodulator {
    pub fn new(mcs: u8) -> Self {
        let mut d = Self {
            mcs: 0,
            num_carriers: 0,
            carriers: [0.0; 8],
            prev_phases: [0.0; 8],
        };
        d.reset_for_mcs(mcs);
        d
    }

    pub fn reset_for_mcs(&mut self, mcs: u8) {
        self.mcs = mcs;
        match mcs {
            2 => {
                self.num_carriers = 4;
                self.carriers[0..4].copy_from_slice(&[600.0, 1000.0, 1400.0, 1800.0]);
                self.prev_phases[0..4].fill(0.0);
            }
            3 => {
                self.num_carriers = 8;
                for k in 0..8 {
                    self.carriers[k] = (k as f32 + 3.0) * 200.0;
                }
                self.prev_phases[0..8].fill(0.0);
            }
            4 => {
                self.num_carriers = 8;
                const CARRIERS_MCS4: [f32; 8] = [
                    571.4286,
                    857.1429,
                    1428.5714,
                    1714.2857,
                    2000.0,
                    2285.7144,
                    2571.4287,
                    2857.1428,
                ];
                self.carriers[0..8].copy_from_slice(&CARRIERS_MCS4);
                self.prev_phases[0..8].fill(0.0);
            }
            _ => {
                self.num_carriers = 4;
                self.carriers[0..4].copy_from_slice(&[600.0, 1000.0, 1400.0, 1800.0]);
                self.prev_phases[0..4].fill(0.0);
            }
        }
    }

    pub fn demodulate_frame(
        &mut self,
        samples: &[f32],
        dither: &mut Prbs7Dither,
    ) -> Result<([u8; 64], [f32; 64]), &'static str> {
        let (n_sym, total_symbols) = if self.mcs == 2 { (80, 65) } else { (40, 33) };
        if samples.len() < total_symbols * n_sym {
            return Err("Insufficient samples for DQPSK frame");
        }

        let fs = 8000.0f32;
        let z = self.num_carriers;

        // Symbol 0: Reference symbol - capture reference phases
        let ref_samples = &samples[0..n_sym];
        for k in 0..z {
            let (mut i_val, mut q_val) = (0.0f32, 0.0f32);
            for n in 0..n_sym {
                let phi = 2.0 * PI * self.carriers[k] * (n as f32 / fs);
                let s = ref_samples[n];
                i_val += s * phi.cos();
                q_val -= s * phi.sin();
            }
            self.prev_phases[k] = q_val.atan2(i_val);
        }

        let mut out_bytes = [0u8; 64];
        let mut out_confidences = [0.0f32; 64];
        let mut bit_accumulator = 0u64;
        let mut bits_collected = 0;
        let mut byte_idx = 0;
        let mut sym_conf_sum;

        let num_data_symbols = total_symbols - 1;
        for m in 1..=num_data_symbols {
            dither.step();
            let sym_samples = &samples[m * n_sym..(m + 1) * n_sym];
            sym_conf_sum = 0.0;

            for k in 0..z {
                let (mut i_val, mut q_val) = (0.0f32, 0.0f32);
                for n in 0..n_sym {
                    let phi = 2.0 * PI * self.carriers[k] * (n as f32 / fs);
                    let s = sym_samples[n];
                    i_val += s * phi.cos();
                    q_val -= s * phi.sin();
                }
                let curr_phase = q_val.atan2(i_val);
                let diff_phase = wrap_2pi(curr_phase - self.prev_phases[k]);
                self.prev_phases[k] = curr_phase;

                // Subtract dither
                let dither_delta = dither.current() - dither.raw_prev;
                let clean_diff = wrap_2pi(diff_phase - dither_delta);

                // Gray decode:
                // 00 -> 0 rad
                // 01 -> +pi/2 rad
                // 11 -> pi rad
                // 10 -> -pi/2 rad
                let (b1, b0, conf) = gray_decode_dqpsk(clean_diff);
                sym_conf_sum += conf;

                bit_accumulator = (bit_accumulator << 2) | ((b1 as u64) << 1) | (b0 as u64);
                bits_collected += 2;

                if bits_collected >= 8 {
                    bits_collected -= 8;
                    let byte_val = ((bit_accumulator >> bits_collected) & 0xFF) as u8;
                    if byte_idx < 64 {
                        out_bytes[byte_idx] = byte_val;
                        out_confidences[byte_idx] = sym_conf_sum / (z as f32);
                        byte_idx += 1;
                    }
                }
            }
        }

        Ok((out_bytes, out_confidences))
    }
}

#[inline]
fn gray_decode_dqpsk(angle: f32) -> (u8, u8, f32) {
    // Decision boundaries:
    // [-pi/4, +pi/4] -> 00 (center 0)
    // [+pi/4, +3pi/4] -> 01 (center pi/2)
    // [+3pi/4, +pi] or [-pi, -3pi/4] -> 11 (center pi)
    // [-3pi/4, -pi/4] -> 10 (center -pi/2)
    let a = angle;
    if (-PI / 4.0..PI / 4.0).contains(&a) {
        let dist = (a - 0.0).abs();
        let conf = (1.0 - dist / (PI / 4.0)).clamp(0.0, 1.0);
        (0, 0, conf)
    } else if (PI / 4.0..3.0 * PI / 4.0).contains(&a) {
        let dist = (a - PI / 2.0).abs();
        let conf = (1.0 - dist / (PI / 4.0)).clamp(0.0, 1.0);
        (0, 1, conf)
    } else if (-3.0 * PI / 4.0..-PI / 4.0).contains(&a) {
        let dist = (a - (-PI / 2.0)).abs();
        let conf = (1.0 - dist / (PI / 4.0)).clamp(0.0, 1.0);
        (1, 0, conf)
    } else {
        let center = if a >= 0.0 { PI } else { -PI };
        let dist = (a - center).abs();
        let conf = (1.0 - dist / (PI / 4.0)).clamp(0.0, 1.0);
        (1, 1, conf)
    }
}

#[inline]
fn raised_cosine_window(n: usize, total: usize, l: usize) -> f32 {
    if n < l {
        0.5 * (1.0 - (PI * (n as f32 + 0.5) / l as f32).cos())
    } else if n >= total - l {
        0.5 * (1.0 - (PI * ((total - 1 - n) as f32 + 0.5) / l as f32).cos())
    } else {
        1.0
    }
}

#[inline]
pub fn wrap_2pi(mut angle: f32) -> f32 {
    while angle > PI {
        angle -= 2.0 * PI;
    }
    while angle <= -PI {
        angle += 2.0 * PI;
    }
    angle
}

// ============================================================================
// GMD Soft-Decision Reed-Solomon Decoding
// ============================================================================

pub fn decode_gmd_canonical_frame(
    interleaved: &[u8; 64],
    confidences: &[f32; 64],
) -> Result<CanonicalDataFrame, &'static str> {
    let mut low_conf: [(f32, usize); 64] = [(0.0, 0); 64];
    let mut low_conf_len = 0;
    for (i, &conf) in confidences.iter().enumerate().take(64) {
        if conf < 0.60 {
            low_conf[low_conf_len] = (conf, i);
            low_conf_len += 1;
        }
    }
    // Stable slice sorting may allocate scratch space on the audio thread.
    // Use the original byte index as a tie-breaker for deterministic, in-place ranking.
    low_conf[..low_conf_len].sort_unstable_by(|a, b|
        a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));

    let trial_counts = [16, 14, 12, 10, 8, 6, 4, 2, 0];
    let mut erasures = [0usize; 16];
    for &e_target in &trial_counts {
        let e_actual = e_target.min(low_conf_len);
        for j in 0..e_actual {
            erasures[j] = low_conf[j].1;
        }

        if let Ok(frame) = CanonicalDataFrame::decode(interleaved, &erasures[..e_actual]) {
            return Ok(frame);
        }
    }

    CanonicalDataFrame::decode(interleaved, &[])
}

// ============================================================================
// Audio Circular Buffer (Wait-Free, Zero-Allocation)
// ============================================================================

pub struct AudioRingBuffer {
    buffer: [i16; AUDIO_RING_CAPACITY],
    write_ptr: usize,
    read_ptr: usize,
}

impl Default for AudioRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRingBuffer {
    pub const fn new() -> Self {
        Self {
            buffer: [0i16; AUDIO_RING_CAPACITY],
            write_ptr: 0,
            read_ptr: 0,
        }
    }

    pub fn available_read(&self) -> usize {
        self.write_ptr.wrapping_sub(self.read_ptr)
    }

    pub fn available_write(&self) -> usize {
        AUDIO_RING_CAPACITY - self.available_read()
    }

    pub fn write_samples(&mut self, samples: &[i16]) -> usize {
        let to_write = samples.len().min(self.available_write());
        for (i, &sample) in samples.iter().enumerate().take(to_write) {
            let idx = (self.write_ptr + i) & (AUDIO_RING_CAPACITY - 1);
            self.buffer[idx] = sample;
        }
        self.write_ptr = self.write_ptr.wrapping_add(to_write);
        to_write
    }

    pub fn read_samples(&mut self, out: &mut [i16]) -> usize {
        let to_read = out.len().min(self.available_read());
        for (i, slot) in out.iter_mut().enumerate().take(to_read) {
            let idx = (self.read_ptr + i) & (AUDIO_RING_CAPACITY - 1);
            *slot = self.buffer[idx];
        }
        self.read_ptr = self.read_ptr.wrapping_add(to_read);
        to_read
    }

    pub fn clear(&mut self) {
        self.write_ptr = 0;
        self.read_ptr = 0;
    }
}

// ============================================================================
// PHY Transmitter & Streaming Receiver
// ============================================================================

pub struct PhyTransmitter {
    pub mcs: u8,
    rms_ceiling: f32,
    burst_floats: Box<[f32; MAX_BURST_SAMPLES]>,
    burst_pcm: Box<[i16; MAX_BURST_SAMPLES]>,
    dqpsk: DqpskModulator,
}

impl PhyTransmitter {
    pub fn new(mcs: u8) -> Self {
        Self {
            mcs,
            rms_ceiling: 1.0,
            burst_floats: vec![0.0f32; MAX_BURST_SAMPLES].into_boxed_slice().try_into().unwrap(),
            burst_pcm: vec![0i16; MAX_BURST_SAMPLES].into_boxed_slice().try_into().unwrap(),
            dqpsk: DqpskModulator::new(mcs),
        }
    }

    /// Host configuration or audio-owner command, applied between bursts.
    pub fn set_rms_ceiling(&mut self, ceiling: f32) -> Result<(), &'static str> {
        if !ceiling.is_finite() || !(0.0..=1.0).contains(&ceiling) {
            return Err("invalid RMS ceiling");
        }
        self.rms_ceiling = ceiling;
        Ok(())
    }

    pub fn reset(&mut self, mcs: u8) {
        self.mcs = mcs;
        self.dqpsk.reset_for_mcs(mcs);
    }

    pub fn modulate_burst(
        &mut self,
        mcs: u8,
        frames: &[CanonicalDataFrame],
        beac_seq: u8,
    ) -> &[i16] {
        self.modulate_fields(Beacon { current_mcs: mcs, requested_mcs: mcs,
            tx_power: 0, sequence: beac_seq, mac: 0 }, frames, false)
    }

    /// Authenticated header supplied by the session's monotonic counter owner.
    /// Only the implemented MCS 2/3 profiles are supported on this path.
    pub fn modulate_authenticated(&mut self, beacon: Beacon,
        frames: &[CanonicalDataFrame], direction: bool) -> Result<&[i16], &'static str> {
        if !matches!(beacon.current_mcs, 2 | 3) || beacon.requested_mcs > 4 ||
            beacon.tx_power > 3 || frames.is_empty() || frames.len() > 8 {
            return Err("unsupported authenticated burst");
        }
        Ok(self.modulate_fields(beacon, frames, direction))
    }

    fn modulate_fields(&mut self, beacon: Beacon, frames: &[CanonicalDataFrame], direction: bool) -> &[i16] {
        if frames.is_empty() { return &[]; }
        let mcs = beacon.current_mcs;
        let beac_seq = beacon.sequence;
        let mut offset = 0;

        // 1. Barker-13 Preamble (520 samples)
        let preamble = synthesize_barker_preamble();
        self.burst_floats[offset..offset + BARKER_TOTAL_SAMPLES].copy_from_slice(&preamble);
        offset += BARKER_TOTAL_SAMPLES;

        // 2. Inter-burst guard silence (0 samples for MCS 3, 80 samples for nominal per SPEC §4.2)
        let guard_inter = if mcs == 3 { PLCP_MCS3_GUARD_INTER } else { PLCP_NOMINAL_GUARD_INTER };
        let guard_post = if mcs == 3 { PLCP_MCS3_GUARD_POST } else { PLCP_NOMINAL_GUARD_POST };

        if guard_inter > 0 {
            self.burst_floats[offset..offset + guard_inter].fill(0.0);
            offset += guard_inter;
        }

        // 3. 2-FSK PLCP Header (3840 samples)
        let beacon_bits = encode_plcp_header_bits(mcs, beacon.requested_mcs, beacon.tx_power, beac_seq, beacon.mac);
        let fsk_header = synthesize_2fsk_header(&beacon_bits);
        self.burst_floats[offset..offset + FSK_TOTAL_SAMPLES].copy_from_slice(&fsk_header);
        offset += FSK_TOTAL_SAMPLES;

        // 4. Post-beacon guard silence (280 samples for MCS 3, 80 samples for nominal per SPEC §4.2)
        self.burst_floats[offset..offset + guard_post].fill(0.0);
        offset += guard_post;

        // 5. Modulate Frames in-place
        self.dqpsk.reset_for_mcs(mcs);
        for (i, frame) in frames.iter().enumerate() {
            let interleaved = frame.encode();
            let mut dither = Prbs7Dither::new(beac_seq, direction as u8, i as u8);
            let frame_samples = if mcs == 2 { 65 * 80 } else { 33 * 40 };
            self.dqpsk.modulate_frame_into(&interleaved, &mut dither, &mut self.burst_floats[offset..offset + frame_samples]);
            offset += frame_samples;
        }

        // 6. Level conditioning & soft saturation limiting
        let target_rms = RMS_TARGET_MCS[mcs.min(4) as usize].min(self.rms_ceiling);
        condition_and_quantize_pcm_into(&self.burst_floats[..offset], target_rms, &mut self.burst_pcm[..offset]);

        &self.burst_pcm[..offset]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhyRxState {
    IdleSearch,
    PlcpHeader { peak_idx: usize },
    PayloadDemod {
        mcs: u8,
        beac_seq: u8,
        frame_idx: usize,
    },
}

pub struct PhyReceiver {
    pub state: PhyRxState,
    pub sample_buffer: Box<[f32; RX_SAMPLE_CAPACITY]>,
    pub sample_count: usize,
    pub detector: BarkerDetector,
    pub dqpsk: DqpskDemodulator,
}

impl Default for PhyReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl PhyReceiver {
    pub fn new() -> Self {
        Self {
            state: PhyRxState::IdleSearch,
            sample_buffer: vec![0.0f32; RX_SAMPLE_CAPACITY].into_boxed_slice().try_into().unwrap(),
            sample_count: 0,
            detector: BarkerDetector::new(),
            dqpsk: DqpskDemodulator::new(2),
        }
    }

    pub fn reset(&mut self) {
        self.state = PhyRxState::IdleSearch;
        self.sample_count = 0;
    }

    pub fn ingest_samples(&mut self, samples: &[i16]) {
        let to_copy = samples.len().min(RX_SAMPLE_CAPACITY - self.sample_count);
        for (i, &sample) in samples.iter().enumerate().take(to_copy) {
            self.sample_buffer[self.sample_count + i] = sample as f32 / 32768.0;
        }
        self.sample_count += to_copy;
    }

    pub fn drain_samples(&mut self, n: usize) {
        let n = n.min(self.sample_count);
        if n > 0 {
            self.sample_buffer.copy_within(n..self.sample_count, 0);
            self.sample_count -= n;
        }
    }

    pub fn process(&mut self, out_frames: &mut [CanonicalDataFrame; 8]) -> usize {
        self.process_with_verifier(out_frames, false, &mut |_| true)
    }

    /// Verify decoded PLCP fields after physical FEC checks, before entering
    /// payload demodulation or changing MCS. Use one persistent direction and
    /// verifier context per session; reset PHY buffering on context changes.
    pub fn process_with_verifier(&mut self, out_frames: &mut [CanonicalDataFrame; 8],
        direction: bool, verify: &mut impl FnMut(Beacon) -> bool) -> usize {
        let mut decoded_frames = 0;

        loop {
            match self.state {
                PhyRxState::IdleSearch => {
                    // Keep each returned batch associated with one verified
                    // beacon. The engine must consume its frames before a later
                    // beacon can replace the retained control/boundary context.
                    if decoded_frames != 0 { break; }
                    if self.sample_count < BARKER_TOTAL_SAMPLES {
                        break;
                    }

                    // Sliding search for Barker correlation peak with local maximum detection
                    let mut found_peak = None;
                    let max_search = self.sample_count - BARKER_TOTAL_SAMPLES;
                    let search_limit = max_search.min(320);

                    for i in 0..=search_limit {
                        let (corr, energy) = self.detector.correlate(&self.sample_buffer[i..i + BARKER_TOTAL_SAMPLES]);
                        // Positive correlation check (corr > 0.60) rejects the -0.7489 sidelobe at offset -3.
                        // Lookahead window (+4 samples) prevents early triggering on rising flanks.
                        if corr > 0.60 && energy > 1.0 {
                            let mut best_i = i;
                            let mut best_corr = corr;
                            let lookahead = (i + 4).min(max_search);
                            for j in (i + 1)..=lookahead {
                                let (c_j, e_j) = self.detector.correlate(&self.sample_buffer[j..j + BARKER_TOTAL_SAMPLES]);
                                if c_j > best_corr && e_j > 1.0 {
                                    best_corr = c_j;
                                    best_i = j;
                                }
                            }
                            found_peak = Some(best_i);
                            break;
                        }
                    }

                    if let Some(peak_idx) = found_peak {
                        // Drop samples before the peak
                        self.drain_samples(peak_idx);
                        // Advance past Barker preamble
                        if self.sample_count >= BARKER_TOTAL_SAMPLES {
                            self.drain_samples(BARKER_TOTAL_SAMPLES);
                            self.state = PhyRxState::PlcpHeader { peak_idx: 0 };
                        } else {
                            break;
                        }
                    } else {
                        // Shift window forward by up to one chunk, preserving sample alignment
                        let drain_len = max_search.min(160);
                        if drain_len > 0 {
                            self.drain_samples(drain_len);
                        }
                        break;
                    }
                }

                PhyRxState::PlcpHeader { .. } => {
                    let min_needed = 4000;
                    if self.sample_count < min_needed {
                        break;
                    }

                    // 1. Evaluate nominal offset 80 (PLCP_NOMINAL_GUARD_INTER) for nominal MCS (2, 4).
                    // In nominal bursts, samples 0..80 are guard silence, and the true 48-bit 2-FSK header
                    // is located at samples 80..3920 followed by 80 samples of post-guard silence (total 4,000 samples).
                    let mut nominal_valid_mcs = None;
                    let prefix_energy: f32 = self.sample_buffer[..80].iter().map(|s| s * s).sum();
                    let header_energy: f32 = self.sample_buffer[..FSK_TOTAL_SAMPLES].iter().map(|s| s * s).sum();
                    let prefix_ratio = prefix_energy / (header_energy / 48.0).max(1e-6);
                    // An MCS3 header begins immediately. Never treat its shifted
                    // bits as a nominal candidate and spend a MAC/replay check on it.
                    if prefix_ratio <= 0.25 && self.sample_count >= PLCP_NOMINAL_GUARD_INTER + FSK_TOTAL_SAMPLES + PLCP_NOMINAL_GUARD_POST {
                        let mut bits = [false; 48];
                        let mut total_conf = 0.0f32;
                        for (b, bit) in bits.iter_mut().enumerate() {
                            let offset = PLCP_NOMINAL_GUARD_INTER + b * FSK_BIT_SAMPLES;
                            let bit_samples: &[f32; 80] = (&self.sample_buffer[offset..offset + 80]).try_into().unwrap();
                            let (demod_bit, conf) = demodulate_2fsk_bit(bit_samples);
                            *bit = demod_bit;
                            total_conf += conf;
                        }

                        let avg_conf = total_conf / 48.0;
                        if avg_conf >= 0.20 {
                            if let Ok((cur_mcs, req_mcs, tx_pwr, beac_seq, beacon_mac8)) = decode_plcp_header(&bits) {
                                if matches!(cur_mcs, 2 | 4) {
                                    nominal_valid_mcs = Some((cur_mcs, req_mcs, tx_pwr, beac_seq, beacon_mac8));
                                }
                            }
                        }
                    }

                    if let Some((cur_mcs, req_mcs, pwr, beac_seq, mac)) = nominal_valid_mcs {
                        if !verify(Beacon { current_mcs: cur_mcs, requested_mcs: req_mcs,
                            tx_power: pwr, sequence: beac_seq, mac }) {
                            self.drain_samples(PLCP_NOMINAL_GUARD_INTER + FSK_TOTAL_SAMPLES + PLCP_NOMINAL_GUARD_POST);
                            self.state = PhyRxState::IdleSearch;
                            continue;
                        }
                        let guard_post = if cur_mcs == 3 {
                            PLCP_MCS3_GUARD_POST
                        } else {
                            PLCP_NOMINAL_GUARD_POST
                        };
                        self.drain_samples(PLCP_NOMINAL_GUARD_INTER + FSK_TOTAL_SAMPLES + guard_post);
                        self.dqpsk.reset_for_mcs(cur_mcs);
                        self.state = PhyRxState::PayloadDemod {
                            mcs: cur_mcs,
                            beac_seq,
                            frame_idx: 0,
                        };
                        continue;
                    }

                    // 2. Evaluate offset 0 for MCS 3 (0 inter-burst guard).
                    // In genuine MCS 3 bursts, samples 0..80 are active FSK tones (not silence),
                    // so bit 0 energy must be non-zero relative to the entire header.
                    let mut mcs3_decoded = None;
                    if self.sample_count >= FSK_TOTAL_SAMPLES {
                        let bit0_energy: f32 = self.sample_buffer[0..80].iter().map(|&s| s * s).sum();
                        let total_energy: f32 = self.sample_buffer[0..FSK_TOTAL_SAMPLES].iter().map(|&s| s * s).sum();
                        let avg_bit_energy = total_energy / 48.0;

                        // Guard against aliased nominal bursts: bit 0 energy must not be silence
                        if bit0_energy >= 0.30 * avg_bit_energy && bit0_energy > 0.001 {
                            let mut bits = [false; 48];
                            let mut total_conf = 0.0f32;
                            for (b, bit) in bits.iter_mut().enumerate() {
                                let offset = b * FSK_BIT_SAMPLES;
                                let bit_samples: &[f32; 80] = (&self.sample_buffer[offset..offset + 80]).try_into().unwrap();
                                let (demod_bit, conf) = demodulate_2fsk_bit(bit_samples);
                                *bit = demod_bit;
                                total_conf += conf;
                            }

                            if total_conf / 48.0 >= 0.20 {
                                if let Ok((cur_mcs, req_mcs, tx_pwr, beac_seq, beacon_mac8)) = decode_plcp_header(&bits) {
                                    if cur_mcs == 3 {
                                        mcs3_decoded = Some((cur_mcs, req_mcs, tx_pwr, beac_seq, beacon_mac8));
                                    }
                                }
                            }
                        }
                    }

                    if let Some((_cur_mcs, req_mcs, pwr, beac_seq, mac)) = mcs3_decoded {
                        if self.sample_count < FSK_TOTAL_SAMPLES + PLCP_MCS3_GUARD_POST {
                            // Genuine MCS 3 header detected, awaiting post-guard silence (4,120 samples total)
                            break;
                        }
                        if !verify(Beacon { current_mcs: 3, requested_mcs: req_mcs,
                            tx_power: pwr, sequence: beac_seq, mac }) {
                            self.drain_samples(FSK_TOTAL_SAMPLES + PLCP_MCS3_GUARD_POST);
                            self.state = PhyRxState::IdleSearch;
                            continue;
                        }
                        self.drain_samples(FSK_TOTAL_SAMPLES + PLCP_MCS3_GUARD_POST);
                        self.dqpsk.reset_for_mcs(3);
                        self.state = PhyRxState::PayloadDemod {
                            mcs: 3,
                            beac_seq,
                            frame_idx: 0,
                        };
                        continue;
                    }

                    // 3. Fall-through: Neither nominal MCS (2, 4) nor MCS 3 matched.
                    // On 2-FSK Golay decode failure or invalid/aliased header (e.g. MCS 6 or 7),
                    // drain samples and reset immediately to IdleSearch without stalling.
                    self.drain_samples(FSK_TOTAL_SAMPLES);
                    self.state = PhyRxState::IdleSearch;
                    continue;
                }

                PhyRxState::PayloadDemod { mcs, beac_seq, frame_idx } => {
                    if decoded_frames == out_frames.len() { break; }
                    let frame_samples = if mcs == 2 { 65 * 80 } else { 33 * 40 };
                    if self.sample_count < frame_samples {
                        break;
                    }

                    let mut dither = Prbs7Dither::new(beac_seq, direction as u8, frame_idx as u8);

                    if let Ok((interleaved, confidences)) = self.dqpsk.demodulate_frame(
                        &self.sample_buffer[0..frame_samples],
                        &mut dither,
                    ) {
                        if let Ok(frame) = decode_gmd_canonical_frame(&interleaved, &confidences) {
                            let yield_turn = (frame.ctrl & 0x08) != 0;
                            if decoded_frames < out_frames.len() {
                                out_frames[decoded_frames] = frame;
                                decoded_frames += 1;
                            }

                            self.drain_samples(frame_samples);
                            if yield_turn {
                                self.state = PhyRxState::IdleSearch;
                            } else {
                                self.state = PhyRxState::PayloadDemod {
                                    mcs,
                                    beac_seq,
                                    frame_idx: frame_idx + 1,
                                };
                            }
                            continue;
                        }
                    }

                    // Demod failed on this frame, advance frame_samples and reset to search
                    self.drain_samples(frame_samples);
                    self.state = PhyRxState::IdleSearch;
                }
            }
        }

        decoded_frames
    }

    pub fn process_frames(&mut self) -> Vec<CanonicalDataFrame> {
        let mut frames = [CanonicalDataFrame::new(); 8];
        let n = self.process(&mut frames);
        frames[..n].to_vec()
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_siphash_test_vector() {
        // Standard SipHash-2-4 test vector with 0-byte input
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ];
        let h = siphash_2_4(&key, b"");
        assert_eq!(h, 0x726fdb47dd0e0e31);
    }

    #[test]
    fn test_barker_preamble_properties() {
        let preamble = synthesize_barker_preamble();
        assert_eq!(preamble.len(), 520);

        // Verify intra-chip step and boundary transition step limits
        let mut max_intra_step = 0.0f32;
        let mut max_boundary_step = 0.0f32;

        for k in 0..13 {
            for m in 0..39 {
                let idx = k * 40 + m;
                let step = (preamble[idx + 1] - preamble[idx]).abs();
                if step > max_intra_step {
                    max_intra_step = step;
                }
            }
            if k < 12 {
                let b_step = (preamble[(k + 1) * 40] - preamble[k * 40 + 39]).abs();
                if b_step > max_boundary_step {
                    max_boundary_step = b_step;
                }
            }
        }

        assert!(max_intra_step <= 1.20, "Intra-chip step {} exceeds 1.20", max_intra_step);
        assert!(max_boundary_step <= 0.30, "Boundary step {} exceeds 0.30", max_boundary_step);

        // Verify matched filter self-correlation
        let detector = BarkerDetector::new();
        let (corr, energy) = detector.correlate(&preamble);
        assert!((corr - 1.0).abs() < 1e-4, "Autocorrelation peak must be ~1.0, got {}", corr);
        assert!(energy > 0.0);
    }

    #[test]
    fn test_prbs7_dither_period() {
        let mut dither = Prbs7Dither::new(42, 0, 1);
        let first_reg = dither.reg;
        let mut period = 0;
        for _ in 0..200 {
            dither.step();
            period += 1;
            if dither.reg == first_reg {
                break;
            }
        }
        assert_eq!(period, 127, "PRBS-7 period must be 127");
    }

    #[test]
    fn test_plcp_header_encode_decode() {
        let cur_mcs = 2;
        let req_mcs = 3;
        let tx_pwr = 1;
        let beac_seq = 0xA5;
        let mac8 = 0x5C;

        let bits = encode_plcp_header_bits(cur_mcs, req_mcs, tx_pwr, beac_seq, mac8);
        assert_eq!(bits.len(), 48);

        let (c_mcs, r_mcs, pwr, b_seq, b_mac) = decode_plcp_header(&bits).unwrap();
        assert_eq!(c_mcs, cur_mcs);
        assert_eq!(r_mcs, req_mcs);
        assert_eq!(pwr, tx_pwr);
        assert_eq!(b_seq, beac_seq);
        assert_eq!(b_mac, mac8);
    }

    #[test]
    fn test_2fsk_synthesis_and_demodulation() {
        let bits = [
            true, false, true, true, false, false, true, false,
            false, true, true, false, true, false, false, true,
            true, true, false, false, false, true, true, false,
            false, false, true, true, true, false, false, true,
            true, false, false, true, false, true, true, false,
            false, true, false, true, true, false, true, false,
        ];
        let pcm = synthesize_2fsk_header(&bits);
        assert_eq!(pcm.len(), 3840);

        for (b, &expected) in bits.iter().enumerate() {
            let offset = b * 80;
            let bit_samples: [f32; 80] = pcm[offset..offset + 80].try_into().unwrap();
            let (demod_bit, conf) = demodulate_2fsk_bit(&bit_samples);
            assert_eq!(demod_bit, expected, "Bit {} mismatch", b);
            assert!(conf > 0.8, "Bit {} confidence too low: {}", b, conf);
        }
    }

    #[test]
    fn test_dqpsk_modulation_and_demodulation_mcs2() {
        let mut orig_frame = CanonicalDataFrame::new();
        orig_frame.ctrl = 0x0A; // MCS 2, reliable, TDD yield
        orig_frame.seq = 77;
        orig_frame.ack_base = 70;
        orig_frame.ack_map = 0x07;
        orig_frame.payload_len = 16;
        orig_frame.payload[..16].copy_from_slice(b"hello_dqpsk_test");

        let interleaved = orig_frame.encode();

        let mut mod_dither = Prbs7Dither::new(12, 0, 0);
        let mut demod_dither = Prbs7Dither::new(12, 0, 0);

        let mut modulator = DqpskModulator::new(2);
        let samples = modulator.modulate_frame(&interleaved, &mut mod_dither);
        assert_eq!(samples.len(), 65 * 80);

        let mut demodulator = DqpskDemodulator::new(2);
        let (demod_bytes, confidences) = demodulator.demodulate_frame(&samples, &mut demod_dither).unwrap();
        assert_eq!(demod_bytes, interleaved);

        for &c in &confidences {
            assert!(c > 0.8, "Confidence too low: {}", c);
        }

        let decoded = decode_gmd_canonical_frame(&demod_bytes, &confidences).unwrap();
        assert_eq!(decoded, orig_frame);
    }

    #[test]
    fn test_phy_transmitter_receiver_burst_loopback() {
        let mut frame0 = CanonicalDataFrame::new();
        frame0.ctrl = 0x02; // TDD yield = 0
        frame0.seq = 10;
        frame0.payload_len = 8;
        frame0.payload[..8].copy_from_slice(b"frame_00");

        let mut frame1 = CanonicalDataFrame::new();
        frame1.ctrl = 0x0A; // TDD yield = 1 (bit 3 set)
        frame1.seq = 11;
        frame1.payload_len = 8;
        frame1.payload[..8].copy_from_slice(b"frame_01");

        let mut tx = PhyTransmitter::new(2);
        let audio_burst = tx.modulate_burst(2, &[frame0, frame1], 42);
        assert!(!audio_burst.is_empty());

        let mut rx = PhyReceiver::new();
        // Ingest audio in 160-sample chunks
        let mut decoded = Vec::new();
        let chunk_size = 160;
        let mut offset = 0;
        while offset < audio_burst.len() {
            let end = (offset + chunk_size).min(audio_burst.len());
            rx.ingest_samples(&audio_burst[offset..end]);
            let frames = rx.process_frames();
            decoded.extend(frames);
            offset = end;
        }

        assert_eq!(decoded.len(), 2, "Expected 2 decoded frames");
        assert_eq!(decoded[0], frame0);
        assert_eq!(decoded[1], frame1);
    }

    #[test]
    fn test_plcp_decode_failure_resets_to_idle_search() {
        let mut rx = PhyReceiver::new();
        // 1. Barker preamble (520 samples)
        let preamble = synthesize_barker_preamble();
        // 2. Inter-burst guard (80 samples)
        let guard_inter = [0.0f32; 80];
        // 3. Corrupt FSK bits (all zeros or high noise)
        let corrupt_fsk = [0.0f32; 3840];
        // 4. Post guard (80 samples)
        let guard_post = [0.0f32; 80];

        let mut samples_f32 = Vec::new();
        samples_f32.extend_from_slice(&preamble);
        samples_f32.extend_from_slice(&guard_inter);
        samples_f32.extend_from_slice(&corrupt_fsk);
        samples_f32.extend_from_slice(&guard_post);

        let samples_i16: Vec<i16> = samples_f32.iter().map(|&s| (s * 32767.0) as i16).collect();
        rx.ingest_samples(&samples_i16);

        let mut frames = [CanonicalDataFrame::new(); 8];
        let n = rx.process(&mut frames);
        assert_eq!(n, 0, "No frames should be decoded on corrupt PLCP");
        assert_eq!(rx.state, PhyRxState::IdleSearch, "Receiver must reset to IdleSearch on PLCP failure");
    }

    #[test]
    fn test_mcs3_guard_timing_exact_waveform_length() {
        let mut frame0 = CanonicalDataFrame::new();
        frame0.ctrl = 0x0A;
        frame0.seq = 1;
        frame0.payload_len = 4;
        frame0.payload[..4].copy_from_slice(b"mcs3");

        let mut tx = PhyTransmitter::new(3);
        let audio_burst = tx.modulate_burst(3, &[frame0], 1);

        // MCS 3 PLCP duration: 520 (Barker) + 0 (guard_inter) + 3840 (FSK) + 280 (guard_post) = 4,640 samples
        // MCS 3 Frame duration: 33 * 40 = 1,320 samples
        // Total waveform length: 4,640 + 1,320 = 5,960 samples
        assert_eq!(
            audio_burst.len(),
            5960,
            "MCS 3 single-frame burst must equal 4,640 (PLCP) + 1,320 (frame) = 5,960 samples"
        );

        // Verify post-beacon guard silence: samples 4360..4640 (280 samples) must be silence (0)
        // Offset 520..4360 is FSK header (3840 samples)
        for (offset, &sample) in audio_burst.iter().take(4640).skip(4360).enumerate() {
            assert_eq!(
                sample, 0,
                "Sample {} in MCS 3 post-beacon guard silence must be 0", 4360 + offset
            );
        }
    }
}
