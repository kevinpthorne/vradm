use vradm_core::phy::{condition_and_quantize_pcm_into, RMS_TARGET_MCS};

#[test]
fn peak_constrained_samples_are_not_compressed_below_nominal_ceiling() {
    let mut pcm = [0; 4];
    condition_and_quantize_pcm_into(&[1.0, -1.0, 0.5, -0.5], 1.0, &mut pcm);
    // Prior unconditional tanh produced about 0.358 FS at the 0.45 FS peak.
    assert_eq!(pcm, [14745, -14745, 7373, -7373]);
}

#[test]
fn rms_limited_signal_preserves_waveform_up_to_quantization() {
    let samples: Vec<f32> = (0..800)
        .map(|n| (2.0 * std::f64::consts::PI * n as f64 / 40.0).sin() as f32)
        .collect();
    let mut pcm = vec![0; samples.len()];
    condition_and_quantize_pcm_into(&samples, 0.2, &mut pcm);
    let amplitude = 0.2f64 * 2.0f64.sqrt();
    for (n, &sample) in pcm.iter().enumerate() {
        let expected = amplitude * (2.0 * std::f64::consts::PI * n as f64 / 40.0).sin();
        assert!((sample as f64 / 32767.0 - expected).abs() <= 0.501 / 32767.0);
    }
    let rms = (pcm
        .iter()
        .map(|&s| (s as f64 / 32767.0).powi(2))
        .sum::<f64>()
        / pcm.len() as f64)
        .sqrt();
    assert!((rms - 0.2).abs() < 0.00001);
}

#[test]
fn multicarrier_constellation_error_is_limited_to_pcm_quantization() {
    use std::f64::consts::PI;
    // Coherent worst-case peaks and staggered phases for four/eight carriers.
    // These are conditioner-only error vectors, not a complete modem TC-10c.
    for (carriers, mcs) in [(4, 2), (8, 3)] {
        for stagger in [false, true] {
            let n = 8000;
            let frequencies: Vec<f64> = (0..carriers).map(|k| 600.0 + 400.0 * k as f64).collect();
            let phases: Vec<f64> = (0..carriers)
                .map(|k| if stagger { k as f64 * PI / 4.0 } else { 0.0 })
                .collect();
            let raw: Vec<f32> = (0..n)
                .map(|i| {
                    frequencies
                        .iter()
                        .zip(&phases)
                        .map(|(&f, &p)| (2.0 * PI * f * i as f64 / 8000.0 + p).cos())
                        .sum::<f64>() as f32
                })
                .collect();
            let peak = raw.iter().map(|s| (*s as f64).abs()).fold(0.0, f64::max);
            let gain =
                (RMS_TARGET_MCS[mcs] as f64 / (carriers as f64 / 2.0).sqrt()).min(0.45 / peak);
            let mut pcm = vec![0; n];
            condition_and_quantize_pcm_into(&raw, RMS_TARGET_MCS[mcs], &mut pcm);
            let mut error = 0.0;
            let mut reference = 0.0;
            for (&f, &phase) in frequencies.iter().zip(&phases) {
                let mut re = 0.0;
                let mut im = 0.0;
                for (i, &sample) in pcm.iter().enumerate() {
                    let angle = 2.0 * PI * f * i as f64 / 8000.0;
                    re += sample as f64 / 32767.0 * angle.cos() * 2.0 / n as f64;
                    im -= sample as f64 / 32767.0 * angle.sin() * 2.0 / n as f64;
                }
                error += (re - gain * phase.cos()).powi(2) + (im - gain * phase.sin()).powi(2);
                reference += gain * gain;
            }
            let evm = (error / reference).sqrt();
            assert!(evm < 0.001, "MCS{mcs}, stagger={stagger}, EVM={evm}");
            for (actual, input) in pcm.iter().zip(&raw) {
                assert!((*actual as f64 / 32767.0 - *input as f64 * gain).abs() < 0.502 / 32767.0);
                assert!(actual.unsigned_abs() <= 14746);
            }
        }
    }
}

#[test]
fn nonfinite_input_or_invalid_target_silences_only_addressed_output() {
    for input in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut out = [123; 5];
        assert_eq!(
            condition_and_quantize_pcm_into(&[0.1, input, -0.1], 0.2, &mut out),
            3
        );
        assert_eq!(out, [0, 0, 0, 123, 123]);
    }
    for target in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1] {
        let mut out = [123; 2];
        assert_eq!(
            condition_and_quantize_pcm_into(&[1.0, -1.0, 0.5], target, &mut out),
            2
        );
        assert_eq!(out, [0; 2]);
    }
    assert_eq!(
        condition_and_quantize_pcm_into(&[f32::NAN], 0.2, &mut []),
        0
    );
}

#[test]
fn finite_extreme_input_keeps_relative_amplitude_and_safe_peaks() {
    let mut pcm = [0; 4];
    condition_and_quantize_pcm_into(
        &[f32::MAX, -f32::MAX, f32::MAX / 2.0, -f32::MAX / 2.0],
        1.0,
        &mut pcm,
    );
    assert_eq!(pcm, [14745, -14745, 7373, -7373]);
    for amplitude in [1e-4, 0.3, 1.0, 17.3, 1000.0, 1e20] {
        condition_and_quantize_pcm_into(
            &[amplitude, -amplitude, amplitude / 2.0, -amplitude / 2.0],
            1.0,
            &mut pcm,
        );
        assert_eq!(pcm, [14745, -14745, 7373, -7373]);
    }
    condition_and_quantize_pcm_into(&[1.0, -1.0, 0.5, -0.5], 0.0, &mut pcm);
    assert_eq!(pcm, [0; 4]);
}
