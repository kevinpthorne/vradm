use vradm_core::{ccf_phy::*, framing::CompactControlFrame, security::*};

fn keys() -> SessionKeys {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
}
fn wire() -> [u8; 16] {
    keys()
        .sign_ccf(
            0,
            CompactControlFrame {
                ccf_ctrl: 0xba,
                ack_base: 255,
                ack_map: 3,
                ccf_mac: 0,
            },
        )
        .unwrap()
}
fn pcm(wire: [u8; 16]) -> Vec<i16> {
    let mut out = vec![0; CCF_PCM_SAMPLES];
    assert_eq!(
        CcfPitchTransmitter::new(wire).render(&mut out),
        CCF_PCM_SAMPLES
    );
    out
}
fn receive(input: &[i16]) -> UnverifiedCcf {
    CcfPitchReceiver::at_frame_start()
        .push(input)
        .frame
        .unwrap()
        .unwrap()
}
fn owners() -> (ControlTx, ControlRx) {
    let mut tx = ControlTx::new(keys());
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
    (tx, ControlRx::new(keys(), 0))
}

#[test]
fn all_pitches_survive_gain_polarity_and_discarded_prefix() {
    let input = pcm(core::array::from_fn(|i| ((i as u8) << 4) | i as u8));
    for (i, chunk) in input.chunks_exact(400).enumerate() {
        let mut symbol: [i16; 400] = chunk.try_into().unwrap();
        assert_eq!(decode_pitch(&symbol).symbol, (i / 2) as u8);
        for value in &mut symbol {
            *value = -*value / 8;
        }
        for (n, value) in symbol[..120].iter_mut().enumerate() {
            *value = if n % 2 == 0 { i16::MAX } else { i16::MIN };
        }
        let decision = decode_pitch(&symbol);
        assert!(decision.confidence > 0.9, "{i}: {decision:?}");
        assert_eq!(decision.symbol, (i / 2) as u8);
        symbol[120..].fill(0);
        assert_eq!(decode_pitch(&symbol).confidence, 0.0);
    }
}

#[test]
fn chunking_and_explicit_frame_boundaries_preserve_authenticated_response() {
    let mut input = pcm(wire());
    input.extend_from_slice(&[123; 73]);
    for size in [1, 79, 160, 511, 13000] {
        let mut rx = CcfPitchReceiver::at_frame_start();
        let mut pos = 0;
        let frame = loop {
            let result = rx.push(&input[pos..(pos + size).min(input.len())]);
            pos += result.consumed;
            if let Some(frame) = result.frame {
                break frame.unwrap();
            }
            assert!(result.consumed > 0);
        };
        assert_eq!(pos, CCF_PCM_SAMPLES);
        assert_eq!(frame.codeword(), wire());
        assert!(frame.erasures().is_empty());
        assert_eq!(rx.push(&input[pos..]).consumed, 0);
        let (mut tx, mut verifier) = owners();
        let verified = verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 1600)
            .unwrap();
        assert!(verified.apply_semantics);
        assert_eq!(verified.commit_sequence, Some(0));
        rx.reset_to_frame_start();
        assert!(rx.push(&input).frame.unwrap().is_ok());
    }
}

#[test]
fn eight_byte_erasures_recover_and_both_nibbles_count_once() {
    let mut input = pcm(wire());
    for byte in [0, 2, 4, 6, 8, 10, 12, 14] {
        input[byte * 800..byte * 800 + 800].fill(0);
    }
    let frame = receive(&input);
    assert_eq!(frame.erasures(), &[0, 2, 4, 6, 8, 10, 12, 14]);
    let (mut tx, mut verifier) = owners();
    assert!(
        verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 1600)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn excess_erasures_wait_for_entire_frame_and_do_not_hold_previous_pitch() {
    let mut input = pcm(wire());
    for byte in 0..9 {
        input[byte * 800 + 400..byte * 800 + 800].fill(0);
    }
    let mut rx = CcfPitchReceiver::at_frame_start();
    let first = rx.push(&input[..CCF_PCM_SAMPLES - 1]);
    assert!(first.frame.is_none());
    assert_eq!(first.consumed, CCF_PCM_SAMPLES - 1);
    assert_eq!(
        rx.push(&input[CCF_PCM_SAMPLES - 1..])
            .frame
            .unwrap()
            .unwrap_err(),
        CcfPcmError::TooManyErasures
    );
}

#[test]
fn mixed_unknown_errors_and_erasures_are_corrected() {
    let mut damaged = wire();
    damaged[3] ^= 0x37;
    damaged[11] ^= 0xa9;
    let mut input = pcm(damaged);
    for byte in [0, 5, 9, 15] {
        input[byte * 800..byte * 800 + 400].fill(0);
    }
    let frame = receive(&input);
    let (mut tx, mut verifier) = owners();
    assert!(
        verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 1600)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn channel_integrity_does_not_replace_mac_verification() {
    let mut forged = CompactControlFrame::decode(wire(), &[]).unwrap();
    forged.ccf_mac ^= 1;
    let frame = receive(&pcm(forged.encode()));
    let (mut tx, mut verifier) = owners();
    assert_eq!(
        verifier
            .verify_control_response(&mut tx, frame.codeword(), frame.erasures(), 1600)
            .unwrap_err(),
        SecurityError::BadMac
    );
    assert_eq!(verifier.mac_failures(), 1);
    let good = receive(&pcm(wire()));
    assert!(
        verifier
            .verify_control_response(&mut tx, good.codeword(), good.erasures(), 3200)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn transmitter_resets_and_zero_fills_tail() {
    let expected = pcm(wire());
    let mut tx = CcfPitchTransmitter::new(wire());
    let mut actual = Vec::new();
    let mut chunk = [99; 511];
    while tx.remaining_samples() > 0 {
        let count = tx.render(&mut chunk);
        actual.extend_from_slice(&chunk[..count]);
        assert!(chunk[count..].iter().all(|&v| v == 0));
    }
    assert_eq!(actual, expected);
    assert!(actual.iter().all(|v| v.unsigned_abs() <= 14746));
    assert_eq!(tx.render(&mut chunk), 0);
    assert_eq!(chunk, [0; 511]);
    tx.reset(wire());
    assert_eq!(tx.render(&mut actual), CCF_PCM_SAMPLES);
    assert_eq!(actual, expected);
}

#[test]
fn deterministic_noise_and_dc_are_erasures_but_moderate_noise_retains_pitches() {
    let input = pcm(core::array::from_fn(|i| ((i as u8) << 4) | i as u8));
    let mut seed = 19u32;
    for (i, chunk) in input.chunks_exact(400).enumerate() {
        let mut symbol: [i16; 400] = chunk.try_into().unwrap();
        let mut noise = [0; 400];
        for n in 0..400 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            noise[n] = (seed >> 16) as i16;
            symbol[n] = symbol[n] / 2 + noise[n] / 32 + 1000;
        }
        let decision = decode_pitch(&symbol);
        assert!(decision.confidence > 0.9);
        assert_eq!(decision.symbol, (i / 2) as u8);
        assert_eq!(decode_pitch(&noise).confidence, 0.0);
    }
    assert_eq!(decode_pitch(&[1000; 400]).confidence, 0.0);
}

#[test]
fn audible_channel_valid_response_produces_deferred_mcs_plan() {
    use vradm_core::mcs_control::McsNegotiator;
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut policy = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    policy.request_upshift(3, 0, true, 5000, 0).unwrap();
    let frame = receive(&pcm(wire()));
    policy
        .receive(frame.codeword(), frame.erasures(), 1600)
        .unwrap();
    assert_eq!(policy.current_mcs(), 2);
    policy.complete_commit(1600).unwrap();
    assert_eq!(policy.current_mcs(), 3);
}

#[test]
fn wrong_sync_and_uncorrectable_codewords_fail_channel_admission() {
    let mut invalid = wire();
    invalid.fill(0xff);
    assert_eq!(
        CcfPitchReceiver::at_frame_start()
            .push(&pcm(invalid))
            .frame
            .unwrap()
            .unwrap_err(),
        CcfPcmError::ChannelIntegrity
    );
}
