use vradm_core::fec::*;
use vradm_core::framing::*;
use vradm_core::arq::*;
use vradm_core::c_abi::*;

// ============================================================================
// Simple deterministic PRNG for test reproducibility without external crates
// ============================================================================
struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        Self { state: if seed == 0 { 0x853c49e6748fea9b } else { seed } }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.state >> 32) as u32
    }

    fn next_bounded(&mut self, bound: usize) -> usize {
        (self.next_u32() as usize) % bound
    }

    fn next_u8(&mut self) -> u8 {
        (self.next_u32() & 0xFF) as u8
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f32) / (u32::MAX as f32)
    }

    // Standard Box-Muller transform for zero-mean, unit-variance Gaussian noise
    fn next_gaussian(&mut self) -> f32 {
        let u1 = self.next_f32().max(1e-7);
        let u2 = self.next_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

// ============================================================================
// SUITE 1: REED-SOLOMON RS(64,48) ERROR CORRECTION CAPABILITY STRESS
// ============================================================================

#[test]
fn test_rs_64_48_parametric_error_capability() {
    let mut rng = TestRng::new(0x123456789ABCDEF0);

    for error_count in 1..=8 {
        let mut pass_count = 0;
        let trials = 100;

        for _ in 0..trials {
            let mut info = [0u8; 48];
            for b in info.iter_mut() {
                *b = rng.next_u8();
            }

            let original_codeword = rs_encode_64_48(&info);
            let mut corrupted_codeword = original_codeword;

            // Pick `error_count` distinct positions in 0..64
            let mut positions = Vec::new();
            while positions.len() < error_count {
                let pos = rng.next_bounded(64);
                if !positions.contains(&pos) {
                    positions.push(pos);
                }
            }

            // Inject non-zero error at each chosen position
            for &pos in &positions {
                let mut err = rng.next_u8();
                if err == 0 {
                    err = 1;
                }
                corrupted_codeword[pos] ^= err;
            }

            // rs_decode_64_48 with 0 erasures
            let res = rs_decode_64_48(&mut corrupted_codeword, &[]);
            assert!(res.is_ok(), "Failed to correct {} byte errors at positions {:?}", error_count, positions);
            assert_eq!(corrupted_codeword, original_codeword, "Recovered codeword mismatch for {} errors", error_count);
            pass_count += 1;
        }

        assert_eq!(pass_count, trials, "Expected 100% success for {} byte errors", error_count);
    }
}

#[test]
fn test_rs_64_48_exact_boundary_8_byte_errors() {
    let mut info = [0u8; 48];
    for (i, b) in info.iter_mut().enumerate() {
        *b = ((i * 17 + 5) & 0xFF) as u8;
    }
    let original = rs_encode_64_48(&info);

    // Case 1: First 8 bytes corrupted (pos 0..8)
    let mut c1 = original;
    for i in 0..8 {
        c1[i] ^= (i as u8) + 1;
    }
    assert!(rs_decode_64_48(&mut c1, &[]).is_ok());
    assert_eq!(c1, original, "First 8 bytes recovery failed");

    // Case 2: Last 8 parity bytes corrupted (pos 56..64)
    let mut c2 = original;
    for i in 56..64 {
        c2[i] ^= i as u8;
    }
    assert!(rs_decode_64_48(&mut c2, &[]).is_ok());
    assert_eq!(c2, original, "Last 8 bytes recovery failed");

    // Case 3: Split 4 info bytes + 4 parity bytes (pos 0..4 and pos 60..64)
    let mut c3 = original;
    for i in 0..4 {
        c3[i] ^= 0xAA;
    }
    for i in 60..64 {
        c3[i] ^= 0x55;
    }
    assert!(rs_decode_64_48(&mut c3, &[]).is_ok());
    assert_eq!(c3, original, "Split 4+4 bytes recovery failed");

    // Case 4: Alternating positions (0, 2, 4, 6, 8, 10, 12, 14)
    let mut c4 = original;
    for i in (0..16).step_by(2) {
        c4[i] ^= 0xFF;
    }
    assert!(rs_decode_64_48(&mut c4, &[]).is_ok());
    assert_eq!(c4, original, "Alternating 8 bytes recovery failed");
}

#[test]
fn test_rs_64_48_beyond_capability_9_byte_errors_rejected() {
    let mut rng = TestRng::new(0x9876543210FEDCBA);
    let mut info = [0u8; 48];
    for b in info.iter_mut() {
        *b = rng.next_u8();
    }
    let original = rs_encode_64_48(&info);

    let mut rejected_count = 0;
    let trials = 50;
    for _ in 0..trials {
        let mut corrupted = original;
        let mut positions = Vec::new();
        while positions.len() < 9 {
            let pos = rng.next_bounded(64);
            if !positions.contains(&pos) {
                positions.push(pos);
            }
        }
        for &pos in &positions {
            let mut err = rng.next_u8();
            if err == 0 {
                err = 0xAA;
            }
            corrupted[pos] ^= err;
        }

        let res = rs_decode_64_48(&mut corrupted, &[]);
        if res.is_err() {
            rejected_count += 1;
        } else {
            assert_ne!(corrupted, original);
        }
    }

    assert!(rejected_count >= 45, "Expected >= 90% explicit rejection for 9-byte errors, got {}/{}", rejected_count, trials);
}

#[test]
fn test_rs_64_48_mixed_erasures_and_errors() {
    let mut rng = TestRng::new(0xBADC0FFEE0123456);
    let mut info = [0u8; 48];
    for b in info.iter_mut() {
        *b = rng.next_u8();
    }
    let original = rs_encode_64_48(&info);

    let test_cases = [
        (16, 0), // 16 erasures, 0 errors: 2*0 + 16 = 16
        (14, 1), // 14 erasures, 1 error: 2*1 + 14 = 16
        (12, 2), // 12 erasures, 2 errors: 2*2 + 12 = 16
        (10, 3), // 10 erasures, 3 errors: 2*3 + 10 = 16
        (8, 4),  // 8 erasures, 4 errors: 2*4 + 8 = 16
        (6, 5),  // 6 erasures, 5 errors: 2*5 + 6 = 16
        (4, 6),  // 4 erasures, 6 errors: 2*6 + 4 = 16
        (2, 7),  // 2 erasures, 7 errors: 2*7 + 2 = 16
        (0, 8),  // 0 erasures, 8 errors: 2*8 + 0 = 16
    ];

    for &(num_erasures, num_errors) in &test_cases {
        let mut corrupted = original;

        let mut erasures = Vec::new();
        while erasures.len() < num_erasures {
            let pos = rng.next_bounded(64);
            if !erasures.contains(&pos) {
                erasures.push(pos);
            }
        }

        for &pos in &erasures {
            corrupted[pos] ^= rng.next_u8().max(1);
        }

        let mut errors = Vec::new();
        while errors.len() < num_errors {
            let pos = rng.next_bounded(64);
            if !erasures.contains(&pos) && !errors.contains(&pos) {
                errors.push(pos);
            }
        }

        for &pos in &errors {
            corrupted[pos] ^= rng.next_u8().max(1);
        }

        let res = rs_decode_64_48(&mut corrupted, &erasures);
        assert!(
            res.is_ok(),
            "Failed mixed case: {} erasures + {} errors (2v+e={})",
            num_erasures, num_errors, 2 * num_errors + num_erasures
        );
        assert_eq!(corrupted, original, "Codeword mismatch for {} erasures + {} errors", num_erasures, num_errors);
    }

    // Boundary breach: 17 erasures must fail
    let mut c_excess = original;
    let excess_erasures: Vec<usize> = (0..17).collect();
    assert!(rs_decode_64_48(&mut c_excess, &excess_erasures).is_err(), "17 erasures should fail");
}

#[test]
fn test_canonical_frame_sync_word_corruption_and_max_errors() {
    let mut frame = CanonicalDataFrame::new();
    frame.ctrl = 0x0A;
    frame.seq = 42;
    frame.ack_base = 40;
    frame.ack_map = 0x03;
    frame.payload_len = 30;
    for i in 0..30 {
        frame.payload[i] = (i * 11 + 3) as u8;
    }

    let interleaved = frame.encode();
    let mut rng = TestRng::new(0xCAFEBABEDEADBEEF);

    for additional_errors in 0..=6 {
        let mut corrupted = interleaved;

        corrupted[0] ^= 0x47;
        corrupted[8] ^= 0xB2;

        let mut other_positions = Vec::new();
        while other_positions.len() < additional_errors {
            let pos = rng.next_bounded(64);
            if pos != 0 && pos != 8 && !other_positions.contains(&pos) {
                other_positions.push(pos);
            }
        }

        for &pos in &other_positions {
            corrupted[pos] ^= rng.next_u8().max(1);
        }

        let decoded = CanonicalDataFrame::decode(&corrupted, &[]);
        assert!(
            decoded.is_ok(),
            "Failed to decode frame with 2 sync errors + {} other errors",
            additional_errors
        );
        let decoded_frame = decoded.unwrap();
        assert_eq!(decoded_frame.seq, frame.seq);
        assert_eq!(decoded_frame.ctrl, frame.ctrl);
        assert_eq!(decoded_frame.payload_len, frame.payload_len);
        assert_eq!(&decoded_frame.payload[..30], &frame.payload[..30]);
    }

    // 2 sync errors + 7 other errors = 9 errors -> MUST fail
    let mut corrupted9 = interleaved;
    corrupted9[0] ^= 0x47;
    corrupted9[8] ^= 0xB2;
    for i in 1..=7 {
        corrupted9[i * 8 + 1] ^= 0x77;
    }
    assert!(CanonicalDataFrame::decode(&corrupted9, &[]).is_err());
}

// ============================================================================
// SUITE 2: MULTI-FRAGMENT IP PACKET SLICING & REASSEMBLY UNDER ARQ DROPS
// ============================================================================

#[test]
fn test_arq_slicing_and_reassembly_mtu256_non_zero_frame_drops() {
    let mut packet = [0u8; 256];
    packet[0] = 0x45;
    for i in 1..256 {
        packet[i] = ((i * 31 + 17) & 0xFF) as u8;
    }

    // Test drops for all non-zero frame indices: 1..7
    for drop_idx in 1..7 {
        let mut tx = ArqTransmitter::new();
        let mut rx = ArqReceiver::new();

        tx.enqueue_packet(&packet, false, false).unwrap();
        let frames = tx.get_frames_to_transmit(10);
        assert_eq!(frames.len(), 7);

        for (i, frame) in frames.iter().enumerate() {
            if i != drop_idx {
                let res = rx.receive_frame(frame);
                assert_eq!(res, None);
            }
        }

        tx.on_ack_received(rx.ack_base, rx.ack_map);

        let retransmit_frames = tx.get_frames_to_transmit(10);
        assert_eq!(retransmit_frames.len(), 1, "Must retransmit frame {}", drop_idx);
        assert_eq!(retransmit_frames[0].seq, frames[drop_idx].seq);

        let reassembled = rx.receive_frame(&retransmit_frames[0]);
        assert!(reassembled.is_some());
        assert_eq!(&reassembled.unwrap()[..], &packet[..]);

        tx.on_ack_received(rx.ack_base, rx.ack_map);
        assert_eq!(tx.in_flight.len(), 0);
    }
}

#[test]
fn test_arq_slicing_and_reassembly_max296_non_zero_frame_drops() {
    let mut packet = [0u8; 296];
    packet[0] = 0x45;
    for i in 1..296 {
        packet[i] = ((i * 23 + 41) & 0xFF) as u8;
    }

    for drop_idx in 1..8 {
        let mut tx = ArqTransmitter::new();
        let mut rx = ArqReceiver::new();

        tx.enqueue_packet(&packet, false, false).unwrap();
        let frames = tx.get_frames_to_transmit(10);
        assert_eq!(frames.len(), 8);

        for (i, frame) in frames.iter().enumerate() {
            if i != drop_idx {
                let res = rx.receive_frame(frame);
                assert_eq!(res, None);
            }
        }

        tx.on_ack_received(rx.ack_base, rx.ack_map);

        let retransmit_frames = tx.get_frames_to_transmit(10);
        assert_eq!(retransmit_frames.len(), 1);
        assert_eq!(retransmit_frames[0].seq, frames[drop_idx].seq);

        let reassembled = rx.receive_frame(&retransmit_frames[0]);
        assert!(reassembled.is_some());
        assert_eq!(&reassembled.unwrap()[..], &packet[..]);

        tx.on_ack_received(rx.ack_base, rx.ack_map);
        assert_eq!(tx.in_flight.len(), 0);
    }
}

#[test]
fn test_arq_frame_0_drop_unhandled_defect() {
    // Demonstrates the critical flaw in arq.rs:212-216 where dropping frame 0 causes
    // ArqReceiver to falsely acknowledge frame 0 and frame 1 when frame 1 is received first.
    let mut packet = [0u8; 256];
    packet[0] = 0x45;

    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    tx.enqueue_packet(&packet, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);

    // Drop frame 0, deliver frame 1
    rx.receive_frame(&frames[1]);

    // Resolved: rx.ack_base must not advance past dropped frame 0 (holds at 255)
    // ack_map indicates frame 1 is selectively acknowledged (bit 1 set)
    assert_eq!(rx.ack_base, 255, "ack_base must not advance past dropped frame 0");
    assert_eq!(rx.ack_map, 1 << 1, "ack_map must indicate frame 1 received and frame 0 missing");

    // Transmitter processes this SACK
    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Transmitter correctly schedules Frame 0 for retransmission
    let retransmit = tx.get_frames_to_transmit(10);
    let retransmits_frame_0 = retransmit.iter().any(|f| f.seq == frames[0].seq);
    assert!(retransmits_frame_0, "Transmitter must schedule frame 0 for retransmission");
}

#[test]
fn test_arq_multi_fragment_multiple_drops_and_cascading_retransmissions() {
    let mut packet = [0u8; 256];
    for (i, b) in packet.iter_mut().enumerate() {
        *b = (i ^ 0x5C) as u8;
    }

    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    tx.enqueue_packet(&packet, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);
    assert_eq!(frames.len(), 7);

    // Round 1: Drop frames 1, 3, 5. Deliver frames 0, 2, 4, 6.
    for i in [0, 2, 4, 6] {
        assert_eq!(rx.receive_frame(&frames[i]), None);
    }

    // Verify receiver selective ACK map
    assert_eq!(rx.ack_base, frames[0].seq);
    // Frames 2, 4, 6 have diffs 2, 4, 6 from ack_base -> bits 1, 3, 5 set in ack_map
    let expected_ack_map = (1 << 1) | (1 << 3) | (1 << 5);
    assert_eq!(rx.ack_map, expected_ack_map);

    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Round 2: Transmitter generates retransmissions for 1, 3, 5
    let retrans_r1 = tx.get_frames_to_transmit(10);
    assert_eq!(retrans_r1.len(), 3);
    assert_eq!(retrans_r1[0].seq, frames[1].seq);
    assert_eq!(retrans_r1[1].seq, frames[3].seq);
    assert_eq!(retrans_r1[2].seq, frames[5].seq);

    // Deliver frames 1 and 5, but drop frame 3 AGAIN!
    assert_eq!(rx.receive_frame(&retrans_r1[0]), None); // Frame 1 delivered
    assert_eq!(rx.receive_frame(&retrans_r1[2]), None); // Frame 5 delivered

    // Now ack_base should have advanced past frames 0, 1, 2!
    // seq 0, 1, 2 are all received, so ack_base = 2.
    // seq 3 is missing. seq 4, 5, 6 are received (diffs 2, 3, 4 -> bits 1, 2, 3 in ack_map).
    assert_eq!(rx.ack_base, frames[2].seq);

    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Round 3: Transmitter generates retransmission for frame 3 ONLY
    let retrans_r2 = tx.get_frames_to_transmit(10);
    assert_eq!(retrans_r2.len(), 1);
    assert_eq!(retrans_r2[0].seq, frames[3].seq);

    // Deliver frame 3
    let reassembled = rx.receive_frame(&retrans_r2[0]);
    assert!(reassembled.is_some(), "Reassembly should complete on frame 3");
    let reassembled_pkt = reassembled.unwrap();
    assert_eq!(&reassembled_pkt[..], &packet[..]);

    // All frames should now be cumulatively acknowledged
    assert_eq!(rx.ack_base, frames[6].seq);
    assert_eq!(rx.ack_map, 0);

    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert_eq!(tx.in_flight.len(), 0);
}

#[test]
fn test_arq_out_of_order_and_duplicate_delivery() {
    let mut packet = [0u8; 296];
    for (i, b) in packet.iter_mut().enumerate() {
        *b = ((i * 13) & 0xFF) as u8;
    }

    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    tx.enqueue_packet(&packet, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);

    // Deliver completely in reverse order: 7, 6, 5, 4, 3, 2, 1, 0
    // With duplicates of frame 7 and frame 4 interspersed!
    assert_eq!(rx.receive_frame(&frames[7]), None);
    assert_eq!(rx.receive_frame(&frames[7]), None); // Duplicate 7
    assert_eq!(rx.receive_frame(&frames[6]), None);
    assert_eq!(rx.receive_frame(&frames[5]), None);
    assert_eq!(rx.receive_frame(&frames[4]), None);
    assert_eq!(rx.receive_frame(&frames[4]), None); // Duplicate 4
    assert_eq!(rx.receive_frame(&frames[3]), None);
    assert_eq!(rx.receive_frame(&frames[2]), None);
    assert_eq!(rx.receive_frame(&frames[1]), None);

    // Final frame 0 triggers full reassembly!
    let reassembled = rx.receive_frame(&frames[0]);
    assert!(reassembled.is_some(), "Reverse order delivery must reassemble");
    let reassembled_pkt = reassembled.unwrap();
    assert_eq!(&reassembled_pkt[..], &packet[..]);
}

// ============================================================================
// SUITE 3: AUDIO BUFFER STRESS (AMPLITUDE SCALING, NOISE, SYNC CORRUPTION)
// ============================================================================

#[test]
fn test_audio_buffer_amplitude_scaling_tolerance() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    let test_packet = [0x45, 0x00, 0x00, 0x20, 0x12, 0x34, 0x00, 0x00, 0x40, 0x01, 0x00, 0x00, 10, 99, 0, 2, 10, 99, 0, 1];

    // Scaling factors from -6 dB (0.50x) to +4 dB (1.6x)
    let scale_factors = [0.50f32, 0.707f32, 0.85f32, 1.0f32, 1.25f32, 1.50f32];

    for &scale in &scale_factors {
        let engine_a = unsafe { vradm_create(&config) };
        let engine_b = unsafe { vradm_create(&config) };

        unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

        // Generate audio from Node A
        let mut audio_channel: Vec<i16> = Vec::new();
        let mut chunk = [0i16; 160];
        for _ in 0..120 {
            let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
            if n > 0 {
                audio_channel.extend_from_slice(&chunk[..n as usize]);
            }
            if audio_channel.len() >= 9720 {
                break;
            }
        }

        // Apply amplitude scaling to audio buffer
        let mut scaled_audio = Vec::with_capacity(audio_channel.len());
        for &s in &audio_channel {
            let scaled = ((s as f32) * scale).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            scaled_audio.push(scaled);
        }

        // Stream into Node B
        let mut offset = 0;
        while offset < scaled_audio.len() {
            let chunk_size = 160.min(scaled_audio.len() - offset);
            unsafe {
                vradm_process_audio(engine_b, scaled_audio[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
            }
            offset += chunk_size;
        }

        let mut rx_buf = [0u8; 128];
        let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
        assert_eq!(
            poll_res,
            test_packet.len() as i32,
            "Failed packet decode at amplitude scale {}x",
            scale
        );
        assert_eq!(&rx_buf[..poll_res as usize], &test_packet[..]);

        unsafe {
            vradm_destroy(engine_a);
            vradm_destroy(engine_b);
        }
    }
}

#[test]
fn test_audio_buffer_additive_gaussian_noise_snr_sweep() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    let test_packet = [0x45, 0x00, 0x00, 0x1E, 0xAA, 0xBB, 0x00, 0x00, 0x40, 0x06, 0x00, 0x00, 10, 99, 0, 2, 10, 99, 0, 1, 0x1F, 0x90, 0x00, 0x16];

    // High and moderate SNR levels (30 dB, 25 dB, 20 dB, 16 dB)
    let snr_db_levels = [30.0f32, 25.0f32, 20.0f32, 16.0f32];

    for &snr_db in &snr_db_levels {
        let mut rng = TestRng::new(0x1122334455667788 + (snr_db as u64));
        let engine_a = unsafe { vradm_create(&config) };
        let engine_b = unsafe { vradm_create(&config) };

        unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

        let mut audio_channel: Vec<i16> = Vec::new();
        let mut chunk = [0i16; 160];
        for _ in 0..120 {
            let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
            if n > 0 {
                audio_channel.extend_from_slice(&chunk[..n as usize]);
            }
            if audio_channel.len() >= 9720 {
                break;
            }
        }

        // Measure signal RMS
        let mut sum_sq = 0.0f64;
        for &s in &audio_channel {
            sum_sq += (s as f64) * (s as f64);
        }
        let sig_rms = (sum_sq / audio_channel.len() as f64).sqrt() as f32;

        // Calculate noise standard deviation for target SNR
        let noise_std = sig_rms * 10.0f32.powf(-snr_db / 20.0);

        // Inject AWGN into audio channel
        let mut noisy_audio = Vec::with_capacity(audio_channel.len());
        for &s in &audio_channel {
            let noise = rng.next_gaussian() * noise_std;
            let sample_with_noise = (s as f32 + noise).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            noisy_audio.push(sample_with_noise);
        }

        // Stream noisy audio into Node B
        let mut offset = 0;
        while offset < noisy_audio.len() {
            let chunk_size = 160.min(noisy_audio.len() - offset);
            unsafe {
                vradm_process_audio(engine_b, noisy_audio[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
            }
            offset += chunk_size;
        }

        let mut rx_buf = [0u8; 128];
        let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
        assert_eq!(
            poll_res,
            test_packet.len() as i32,
            "Failed packet decode at SNR {} dB",
            snr_db
        );
        assert_eq!(&rx_buf[..poll_res as usize], &test_packet[..]);

        unsafe {
            vradm_destroy(engine_a);
            vradm_destroy(engine_b);
        }
    }
}

#[test]
fn test_audio_buffer_barker_preamble_corruption_resilience() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    let test_packet = b"preamble_corruption_resilience_test";

    // Test case 1: Corrupt 15% of Barker preamble samples (78 samples out of 520).
    // Barker detector has threshold 0.60, so mild preamble corruption is tolerated!
    {
        let engine_a = unsafe { vradm_create(&config) };
        let engine_b = unsafe { vradm_create(&config) };

        unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

        let mut audio_channel: Vec<i16> = Vec::new();
        let mut chunk = [0i16; 160];
        for _ in 0..120 {
            let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
            if n > 0 {
                audio_channel.extend_from_slice(&chunk[..n as usize]);
            }
            if audio_channel.len() >= 9720 {
                break;
            }
        }

        // Corrupt 70 samples in the Barker preamble (samples 50..120) by zeroing them
        for i in 50..120 {
            audio_channel[i] = 0;
        }

        let mut offset = 0;
        while offset < audio_channel.len() {
            let chunk_size = 160.min(audio_channel.len() - offset);
            unsafe {
                vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
            }
            offset += chunk_size;
        }

        let mut rx_buf = [0u8; 128];
        let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
        assert_eq!(poll_res, test_packet.len() as i32, "Moderate preamble corruption must be tolerated");
        assert_eq!(&rx_buf[..poll_res as usize], test_packet);

        unsafe {
            vradm_destroy(engine_a);
            vradm_destroy(engine_b);
        }
    }

    // Test case 2: Complete preamble destruction (zero all 520 samples).
    // Receiver must safely ignore it (return 0 packets) without crashing or hanging.
    {
        let engine_a = unsafe { vradm_create(&config) };
        let engine_b = unsafe { vradm_create(&config) };

        unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

        let mut audio_channel: Vec<i16> = Vec::new();
        let mut chunk = [0i16; 160];
        for _ in 0..120 {
            let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
            if n > 0 {
                audio_channel.extend_from_slice(&chunk[..n as usize]);
            }
            if audio_channel.len() >= 9720 {
                break;
            }
        }

        // Completely zero the preamble
        for i in 0..520 {
            audio_channel[i] = 0;
        }

        let mut offset = 0;
        while offset < audio_channel.len() {
            let chunk_size = 160.min(audio_channel.len() - offset);
            unsafe {
                vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
            }
            offset += chunk_size;
        }

        let mut rx_buf = [0u8; 128];
        let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
        assert_eq!(poll_res, 0, "Destroyed preamble must result in 0 decoded packets");

        unsafe {
            vradm_destroy(engine_a);
            vradm_destroy(engine_b);
        }
    }
}

// ============================================================================
// SUITE 4: END-TO-END CLOSED-LOOP AUDIO RETRANSMISSION UNDER ACOUSTIC FRAME DROPS
// ============================================================================

#[test]
fn test_audio_burst_premature_resynthesis_defect() {
    // Demonstrates the critical flaw in engine.rs:714-725 where vradm_generate_audio
    // immediately re-synthesizes an unrequested burst when tx_ring.available_read() < 160,
    // rather than yielding the TDD turn with silence.
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };
    let engine = unsafe { vradm_create(&config) };

    // Enqueue 1-fragment IP packet (burst length = 4520 + 5200 = 9720 samples)
    let packet = [0x45; 20];
    unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32) };

    let mut chunk = [0i16; 160];
    // Read exactly 9600 samples (60 chunks of 160)
    for _ in 0..60 {
        unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };
    }

    let mut telem1: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut telem1) };
    assert_eq!(telem1.frames_transmitted, 1, "Exactly 1 frame should be synthesized for initial burst");

    // Exactly 120 samples remain in tx_ring (9720 - 9600 = 120 samples).
    // On chunk 61, the caller requests 160 samples.
    // Verified: The engine drains the remaining 120 samples and zero-pads the last 40 samples,
    // without prematurely synthesizing a 2nd burst.
    unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };

    let mut telem2: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut telem2) };

    // Telemetry shows frames_transmitted remains 1 (no premature resynthesis!)
    assert_eq!(
        telem2.frames_transmitted, 1,
        "Engine should drain tail remainder with silence padding and not synthesize a 2nd burst prematurely!"
    );

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_audio_buffer_bit_error_injection_and_rs_correction() {
    // Tests injecting bit errors directly into the modulated DQPSK audio buffer
    // between vradm_generate_audio and vradm_process_audio, verifying that
    // Reed-Solomon RS(64,48) corrects the symbol errors created by audio distortion.
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    let engine_a = unsafe { vradm_create(&config) };
    let engine_b = unsafe { vradm_create(&config) };

    let test_packet = b"pcm_bit_error_tolerance_test_payload";
    unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

    let mut audio_channel: Vec<i16> = Vec::new();
    let mut chunk = [0i16; 160];
    for _ in 0..120 {
        let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
        if n > 0 {
            audio_channel.extend_from_slice(&chunk[..n as usize]);
        }
        if audio_channel.len() >= 9720 {
            break;
        }
    }

    // Inject localized sample corruptions into the payload portion of the burst
    // (payload starts after PLCP preamble: sample offset 4520).
    // Corrupt 2 DQPSK symbol durations (2 * 80 = 160 samples) with sign flips and max amplitude
    for i in 5000..5160 {
        audio_channel[i] = -audio_channel[i];
    }

    let mut offset = 0;
    while offset < audio_channel.len() {
        let chunk_size = 160.min(audio_channel.len() - offset);
        unsafe {
            vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    let mut telem_b: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine_b, &mut telem_b) };
    println!("Bit error test telem_b: rx={}, plcp_lock={}, crc_fail={}", telem_b.frames_received, telem_b.plcp_carrier_locked, telem_b.crc_failures);

    let mut rx_buf = [0u8; 128];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_res, test_packet.len() as i32, "RS FEC failed to correct audio symbol errors");
    assert_eq!(&rx_buf[..poll_res as usize], test_packet);

    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}
