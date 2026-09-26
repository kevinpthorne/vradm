use vradm_core::{ccf_phy::*, ccf_turn::*, framing::CompactControlFrame, security::SessionKeys};
fn wire() -> [u8; 16] {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
        .sign_ccf(
            0,
            CompactControlFrame {
                ccf_ctrl: 0xba,
                ack_base: 7,
                ack_map: 3,
                ccf_mac: 0,
            },
        )
        .unwrap()
}
fn rendered() -> Vec<i16> {
    let mut tx = CcfTurnTransmitter::new();
    tx.start(wire()).unwrap();
    let mut pcm = vec![0; CCF_TURN_SAMPLES];
    assert_eq!(tx.render(&mut pcm), CCF_TURN_SAMPLES);
    pcm
}

#[test]
fn exact_phase_boundaries_and_guard_duration() {
    let mut tx = CcfTurnTransmitter::new();
    assert_eq!(tx.phase(), CcfTurnPhase::Idle);
    tx.start(wire()).unwrap();
    let mut ccf = vec![0; CCF_PCM_SAMPLES];
    assert_eq!(tx.render(&mut ccf), CCF_PCM_SAMPLES);
    assert_eq!(tx.phase(), CcfTurnPhase::EndOfTurn);
    let decoded = CcfPitchReceiver::at_frame_start()
        .push(&ccf)
        .frame
        .unwrap()
        .unwrap();
    assert_eq!(decoded.codeword(), wire());
    let mut eot = [0; EOT_SAMPLES];
    tx.render(&mut eot);
    assert_eq!(tx.phase(), CcfTurnPhase::Guard);
    let mut guard = [123; TURN_GUARD_SAMPLES - 1];
    tx.render(&mut guard);
    assert_eq!(guard, [0; TURN_GUARD_SAMPLES - 1]);
    assert_eq!(tx.phase(), CcfTurnPhase::Guard);
    assert_eq!(tx.remaining_samples(), 1);
    assert_eq!(tx.render(&mut [123; 2]), 1);
    assert_eq!(tx.phase(), CcfTurnPhase::Idle);
    assert_eq!(tx.remaining_samples(), 0);
}

#[test]
fn arbitrary_chunks_cross_boundaries_without_gaps_and_zero_fill_tail() {
    let expected = rendered();
    for size in [1, 79, 160, 511, CCF_TURN_SAMPLES + 17] {
        let mut tx = CcfTurnTransmitter::new();
        tx.start(wire()).unwrap();
        let mut actual = Vec::new();
        let mut chunk = vec![123; size];
        assert_eq!(tx.render(&mut []), 0);
        while tx.remaining_samples() > 0 {
            let count = tx.render(&mut chunk);
            actual.extend_from_slice(&chunk[..count]);
            assert!(chunk[count..].iter().all(|&v| v == 0));
        }
        assert_eq!(actual, expected);
        assert_eq!(tx.render(&mut chunk), 0);
        assert!(chunk.iter().all(|&v| v == 0));
    }
}

#[test]
fn eot_has_spec_frequencies_rms_and_bounded_peak() {
    let pcm = rendered();
    let eot = &pcm[CCF_PCM_SAMPLES..CCF_PCM_SAMPLES + EOT_SAMPLES];
    let rms = (eot
        .iter()
        .map(|&v| (v as f64 / 32767.0).powi(2))
        .sum::<f64>()
        / EOT_SAMPLES as f64)
        .sqrt();
    let expected = 10f64.powf(-12.0 / 20.0);
    assert!((rms - expected).abs() < 0.00005);
    assert!(eot.iter().all(|v| v.unsigned_abs() < 16384));
    let amplitude = |freq: f64| {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &sample) in eot.iter().enumerate() {
            let phase = 2.0 * std::f64::consts::PI * freq * n as f64 / 8000.0;
            re += sample as f64 / 32767.0 * phase.cos();
            im += sample as f64 / 32767.0 * phase.sin();
        }
        2.0 * (re * re + im * im).sqrt() / EOT_SAMPLES as f64
    };
    for freq in [1400.0, 1800.0] {
        assert!((amplitude(freq) - expected).abs() < 0.00005);
    }
    for freq in [1000.0, 1600.0, 2000.0] {
        assert!(amplitude(freq) < 0.00005);
    }
}

#[test]
fn active_turn_cannot_be_replaced_even_during_guard() {
    let mut tx = CcfTurnTransmitter::new();
    tx.start(wire()).unwrap();
    for count in [1, CCF_PCM_SAMPLES - 1, EOT_SAMPLES] {
        tx.render(&mut vec![0; count]);
        let remaining = tx.remaining_samples();
        assert_eq!(tx.start(wire()), Err(CcfTurnError::Busy));
        assert_eq!(tx.remaining_samples(), remaining);
    }
    tx.render(&mut [0; TURN_GUARD_SAMPLES]);
    assert_eq!(tx.start(wire()), Ok(()));
}

#[test]
fn cancel_silences_future_output_and_restart_has_no_stale_samples() {
    let expected = rendered();
    for offset in [13, CCF_PCM_SAMPLES + 13, CCF_PCM_SAMPLES + EOT_SAMPLES + 13] {
        let mut tx = CcfTurnTransmitter::new();
        tx.start(wire()).unwrap();
        tx.render(&mut vec![0; offset]);
        tx.cancel();
        let mut output = vec![123; CCF_TURN_SAMPLES];
        assert_eq!(tx.render(&mut output), 0);
        assert!(output.iter().all(|&v| v == 0));
        tx.start(wire()).unwrap();
        tx.render(&mut output);
        assert_eq!(output, expected);
    }
}

#[test]
fn malformed_or_non_yielding_local_frames_leave_renderer_idle() {
    let mut tx = CcfTurnTransmitter::new();
    let mut damaged = wire();
    damaged[10] ^= 1;
    assert_eq!(tx.start(damaged), Err(CcfTurnError::InvalidCodeword));
    assert_eq!(tx.phase(), CcfTurnPhase::Idle);
    for (ctrl, map) in [(0x3a, 3), (0xfa, 3), (0xb8, 3), (0xba, 128)] {
        let malformed = CompactControlFrame {
            ccf_ctrl: ctrl,
            ack_base: 7,
            ack_map: map,
            ccf_mac: 0,
        };
        assert_eq!(
            tx.start(malformed.encode()),
            Err(CcfTurnError::InvalidCodeword)
        );
        assert_eq!(tx.phase(), CcfTurnPhase::Idle);
    }
    let mut no_yield = CompactControlFrame::decode(wire(), &[]).unwrap();
    no_yield.ccf_ctrl &= !8;
    assert_eq!(tx.start(no_yield.encode()), Err(CcfTurnError::MissingYield));
    assert_eq!(tx.phase(), CcfTurnPhase::Idle);
    assert_eq!(tx.start(wire()), Ok(()));
}

#[test]
fn receive_waits_through_guard_and_preserves_exact_consumption() {
    let mut pcm = rendered();
    pcm.extend_from_slice(&[999; 123]);
    for size in [1, 79, 160, 511, CCF_TURN_SAMPLES + 123] {
        let mut rx = CcfTurnReceiver::at_frame_start();
        let mut pos = 0;
        assert!(rx.push(&[]).frame.is_none());
        let frame = loop {
            let progress = rx.push(&pcm[pos..(pos + size).min(pcm.len())]);
            pos += progress.consumed;
            if let Some(frame) = progress.frame {
                break frame.unwrap();
            }
            assert!(pos < CCF_TURN_SAMPLES);
        };
        assert_eq!(pos, CCF_TURN_SAMPLES);
        assert_eq!(frame.codeword(), wire());
        assert_eq!(rx.push(&pcm[pos..]).consumed, 0);
        assert!(rx.push(&pcm).frame.is_none());
        rx.reset_to_frame_start();
        assert!(rx.push(&pcm).frame.unwrap().is_ok());
    }
}

fn replace_eot(pcm: &mut [i16], frequencies: &[f64]) {
    for (n, sample) in pcm[CCF_PCM_SAMPLES..CCF_PCM_SAMPLES + EOT_SAMPLES]
        .iter_mut()
        .enumerate()
    {
        *sample = (frequencies
            .iter()
            .map(|&f| (2.0 * std::f64::consts::PI * f * n as f64 / 8000.0 + 0.7).sin())
            .sum::<f64>()
            * 5000.0) as i16;
    }
}

#[test]
fn absent_single_or_wrong_tones_fail_only_after_full_guard() {
    for frequencies in [&[][..], &[1400.0][..], &[1800.0][..], &[1200.0, 2000.0][..]] {
        let mut pcm = rendered();
        replace_eot(&mut pcm, frequencies);
        let mut rx = CcfTurnReceiver::at_frame_start();
        assert!(rx.push(&pcm[..CCF_TURN_SAMPLES - 1]).frame.is_none());
        assert_eq!(
            rx.push(&pcm[CCF_TURN_SAMPLES - 1..])
                .frame
                .unwrap()
                .unwrap_err(),
            CcfTurnReceiveError::MissingEndOfTurn
        );
    }
}

#[test]
fn eot_accepts_phase_polarity_gain_dc_and_moderate_noise() {
    let mut pcm = rendered();
    replace_eot(&mut pcm, &[1400.0, 1800.0]);
    let mut seed = 13u32;
    for sample in &mut pcm[CCF_PCM_SAMPLES..CCF_PCM_SAMPLES + EOT_SAMPLES] {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        *sample = -*sample / 4 + 1000 + ((seed >> 16) as i16) / 256;
    }
    // Guard is time for decay, not a silence admission gate.
    pcm[CCF_PCM_SAMPLES + EOT_SAMPLES..].fill(7000);
    assert!(CcfTurnReceiver::at_frame_start()
        .push(&pcm)
        .frame
        .unwrap()
        .is_ok());
}

#[test]
fn noise_dc_truncated_and_interrupted_eot_are_rejected() {
    for kind in 0..4 {
        let mut pcm = rendered();
        let eot = &mut pcm[CCF_PCM_SAMPLES..CCF_PCM_SAMPLES + EOT_SAMPLES];
        match kind {
            0 => {
                let mut seed = 19u32;
                for s in eot {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    *s = (seed >> 16) as i16;
                }
            }
            1 => eot.fill(1000),
            2 => eot[EOT_SAMPLES - 80..].fill(0),
            _ => eot[560..640].fill(0),
        }
        assert_eq!(
            CcfTurnReceiver::at_frame_start()
                .push(&pcm)
                .frame
                .unwrap()
                .unwrap_err(),
            CcfTurnReceiveError::MissingEndOfTurn
        );
    }
}

#[test]
fn bad_ccf_is_not_rescued_by_eot_and_receiver_can_reset_mid_turn() {
    let mut pcm = rendered();
    pcm[..CCF_PCM_SAMPLES].fill(0);
    let mut rx = CcfTurnReceiver::at_frame_start();
    assert!(rx.push(&pcm[..CCF_TURN_SAMPLES - 1]).frame.is_none());
    assert!(matches!(
        rx.push(&pcm[CCF_TURN_SAMPLES - 1..]).frame.unwrap(),
        Err(CcfTurnReceiveError::Ccf(_))
    ));
    let valid = rendered();
    for count in [27, CCF_PCM_SAMPLES + 27, CCF_TURN_SAMPLES - 27] {
        rx.reset_to_frame_start();
        rx.push(&valid[..count]);
        rx.reset_to_frame_start();
        assert_eq!(rx.push(&valid).frame.unwrap().unwrap().codeword(), wire());
    }
}

#[test]
fn complete_audio_turn_still_requires_matching_authenticated_transaction() {
    use vradm_core::security::*;
    let keys = SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]);
    let mut tx = ControlTx::new(keys.clone());
    let mut verifier = ControlRx::new(keys, 0);
    tx.begin_control(
        ControlRequest {
            current_mcs: 2,
            target_mcs: 3,
            tx_power: 0,
            command: ControlCommand::McsCommitAck,
            yield_turn: true,
            deadline_ms: 5000,
        },
        0,
    )
    .unwrap();
    let mut bad = CompactControlFrame::decode(wire(), &[]).unwrap();
    bad.ccf_mac ^= 1;
    let mut renderer = CcfTurnTransmitter::new();
    renderer.start(bad.encode()).unwrap();
    let mut pcm = vec![0; CCF_TURN_SAMPLES];
    renderer.render(&mut pcm);
    let frame = CcfTurnReceiver::at_frame_start()
        .push(&pcm)
        .frame
        .unwrap()
        .unwrap();
    assert_eq!(
        verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 1900)
            .unwrap_err(),
        SecurityError::BadMac
    );
    let frame = CcfTurnReceiver::at_frame_start()
        .push(&rendered())
        .frame
        .unwrap()
        .unwrap();
    assert!(
        verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 3800)
            .unwrap()
            .apply_semantics
    );
}
