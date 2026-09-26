use std::f32::consts::PI;
use vradm_core::arq::*;
use vradm_core::c_abi::*;
use vradm_core::framing::*;
use vradm_core::phy::*;

// ============================================================================
// Deterministic PRNG for rigorous stress testing
// ============================================================================
struct ChallengeRng {
    state: u64,
}

impl ChallengeRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0xA5A5A5A55A5A5A5A } else { seed },
        }
    }

    fn next_u32(&mut self) -> u32 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.state >> 32) as u32
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
        (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos()
    }
}

// ============================================================================
// SUITE 1: FRAME 0 LOSS ACROSS MULTIPLE PACKET BURSTS
// ============================================================================

#[test]
fn challenge_frame_0_loss_consecutive_packet_bursts() {
    let mut rng = ChallengeRng::new(0x202609210001);
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    // 6 consecutive packet bursts of varying sizes up to 7 fragments (250 bytes)
    // where Frame 0 is ALWAYS dropped on initial transmission.
    let packet_sizes = [50, 90, 130, 170, 210, 250];

    for (burst_idx, &size) in packet_sizes.iter().enumerate() {
        let mut packet = vec![0u8; size];
        for b in packet.iter_mut() {
            *b = rng.next_u8();
        }
        packet[0] = 0x45; // IPv4 marker

        tx.enqueue_packet(&packet, false, false).unwrap();
        let frames = tx.get_frames_to_transmit(10);
        assert!(
            frames.len() >= 2,
            "Burst {} with size {} should produce >= 2 frames",
            burst_idx,
            size
        );

        let initial_frame0_seq = frames[0].seq;

        // Deliver frames 1..N, DROPPING Frame 0
        let mut early_reassembly = None;
        for (i, frame) in frames.iter().enumerate() {
            if i == 0 {
                continue; // Drop Frame 0
            }
            if let Some(reassembled) = rx.receive_frame(frame) {
                early_reassembly = Some(reassembled);
            }
        }

        assert!(
            early_reassembly.is_none(),
            "Burst {}: Receiver must not reassemble packet before Frame 0 arrives",
            burst_idx
        );

        // Verify receiver selective ACK state: ack_base holds, ack_map records non-zero frames
        let diff = seq_diff(frames[1].seq, rx.ack_base);
        assert!(
            diff >= 2,
            "Burst {}: ack_base must not advance past missing Frame 0",
            burst_idx
        );
        assert_ne!(
            rx.ack_map, 0,
            "Burst {}: ack_map must record received frames",
            burst_idx
        );

        // Ingest ACK/SACK into Transmitter
        tx.on_ack_received(rx.ack_base, rx.ack_map);

        // Transmitter must retransmit missing Frame 0
        let retransmit_frames = tx.get_frames_to_transmit(10);
        let retransmits_f0 = retransmit_frames.iter().any(|f| f.seq == initial_frame0_seq);
        assert!(
            retransmits_f0,
            "Burst {}: Transmitter must retransmit missing Frame 0",
            burst_idx
        );

        // Deliver retransmitted Frame 0 to Receiver
        let frame_0 = retransmit_frames
            .iter()
            .find(|f| f.seq == initial_frame0_seq)
            .unwrap();
        let delivered_packet = rx.receive_frame(frame_0);

        assert!(
            delivered_packet.is_some(),
            "Burst {}: Receiver must successfully deliver packet upon Frame 0 arrival",
            burst_idx
        );
        assert_eq!(
            delivered_packet.unwrap(),
            packet,
            "Burst {}: Delivered packet bytes must match transmitted packet",
            burst_idx
        );

        // Cumulative ACK advancement
        tx.on_ack_received(rx.ack_base, rx.ack_map);
        assert_eq!(
            tx.in_flight.len(),
            0,
            "Burst {}: Transmitter in-flight queue must be clear",
            burst_idx
        );
        assert_eq!(
            rx.ack_map, 0,
            "Burst {}: Receiver ack_map must be zeroed",
            burst_idx
        );
    }
}

#[test]
fn challenge_defect_arq_sack_overflow_8_fragments() {
    // EMPIRICAL CHALLENGE: When a packet requires 8 fragments (280 bytes),
    // dropping Frame 0 causes Frame 7 to have diff = 8 from ack_base.
    // Because ACK_MAP only has 7 forward bits (diffs 1..7, SPEC §2.1 line 118),
    // Frame 7 cannot be selectively acknowledged. This test empirically proves
    // that Frame 7 remains unacknowledged in tx.in_flight even after Frame 0 is retransmitted.
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    let packet = [0x5Au8; 280]; // Exactly 8 fragments
    tx.enqueue_packet(&packet, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);
    assert_eq!(frames.len(), 8, "280-byte packet must produce exactly 8 fragments");

    let f0_seq = frames[0].seq;
    let _f7_seq = frames[7].seq;

    // Deliver frames 1..7, DROP Frame 0
    for i in 1..8 {
        assert_eq!(rx.receive_frame(&frames[i]), None);
    }

    // Notice: Frame 7 had diff 8 from ack_base.
    // Bits [6..0] of ack_map cover diffs 1..7.
    // Bit for diff 8 does not exist!
    println!("rx.ack_base: {}, rx.ack_map: 0b{:08b}", rx.ack_base, rx.ack_map);

    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Retransmit Frame 0
    let retx = tx.get_frames_to_transmit(10);
    let f0 = retx.iter().find(|f| f.seq == f0_seq).unwrap();
    let delivered = rx.receive_frame(f0);
    assert!(delivered.is_some(), "Packet reassembles on Frame 0 delivery");

    // Cumulative ACK ingestion
    tx.on_ack_received(rx.ack_base, rx.ack_map);

    assert_eq!(
        tx.in_flight.len(),
        0,
        "Frame 7 (diff 8) must be acknowledged after Frame 0 retransmission, clearing in_flight!"
    );
}

#[test]
fn challenge_frame_0_loss_double_drop_and_multi_round_retransmission() {
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    let packet = [0x5Au8; 250];
    tx.enqueue_packet(&packet, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);
    assert_eq!(frames.len(), 7);

    let f0_seq = frames[0].seq;

    // Round 1: Deliver frames 1..6, DROP frame 0
    for i in 1..7 {
        assert_eq!(rx.receive_frame(&frames[i]), None);
    }
    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Round 2: Transmitter schedules Frame 0 retransmission. Drop it again!
    let retransmit1 = tx.get_frames_to_transmit(10);
    assert!(retransmit1.iter().any(|f| f.seq == f0_seq));
    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Round 3: Transmitter schedules Frame 0 retransmission again.
    let retransmit2 = tx.get_frames_to_transmit(10);
    assert!(retransmit2.iter().any(|f| f.seq == f0_seq));

    // Deliver Frame 0
    let f0 = retransmit2.iter().find(|f| f.seq == f0_seq).unwrap();
    let delivered = rx.receive_frame(f0);

    assert!(delivered.is_some(), "Packet must reassemble on 3rd attempt");
    assert_eq!(delivered.unwrap(), &packet[..]);

    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert_eq!(tx.in_flight.len(), 0);
}

#[test]
fn challenge_frame_0_loss_at_seq_wraparound_boundary() {
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    // Advance sequence numbers close to 255
    for _ in 0..250 {
        let dummy = [0x11u8; 30];
        tx.enqueue_packet(&dummy, false, false).unwrap();
        let f = tx.get_frames_to_transmit(1);
        let res = rx.receive_frame(&f[0]);
        assert!(res.is_some());
        tx.on_ack_received(rx.ack_base, rx.ack_map);
    }
    assert_eq!(tx.next_seq, 250);
    assert_eq!(rx.ack_base, 249);

    // Send a 4-fragment packet: seqs 250, 251, 252, 253
    let p1 = [0x22u8; 130];
    tx.enqueue_packet(&p1, false, false).unwrap();
    let frames1 = tx.get_frames_to_transmit(10);
    for f in &frames1 {
        rx.receive_frame(f);
    }
    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert_eq!(tx.next_seq, 254);
    assert_eq!(rx.ack_base, 253);

    // Multi-fragment packet spanning across 255 -> 0:
    // Seqs: 254 (frag 0), 255 (frag 1), 0 (frag 2), 1 (frag 3)
    let p_wrap = [0x33u8; 130];
    tx.enqueue_packet(&p_wrap, false, false).unwrap();
    let frames_wrap = tx.get_frames_to_transmit(10);
    assert_eq!(frames_wrap[0].seq, 254);
    assert_eq!(frames_wrap[1].seq, 255);
    assert_eq!(frames_wrap[2].seq, 0);
    assert_eq!(frames_wrap[3].seq, 1);

    // DROP Frame 0 (seq 254)! Deliver frames 1, 2, 3 (seqs 255, 0, 1)
    for f in &frames_wrap[1..] {
        assert_eq!(rx.receive_frame(f), None);
    }

    // Verify ACK tracking across 255 wraparound
    assert_eq!(rx.ack_base, 253, "ack_base must hold before dropped frame 254");
    let expected_map = (1 << 1) | (1 << 2) | (1 << 3);
    assert_eq!(rx.ack_map, expected_map, "Selective ACK map must track across 255 wraparound");

    tx.on_ack_received(rx.ack_base, rx.ack_map);
    let retx = tx.get_frames_to_transmit(10);
    assert!(retx.iter().any(|f| f.seq == 254));

    // Deliver retransmitted Frame 0 (seq 254)
    let retx_f0 = retx.iter().find(|f| f.seq == 254).unwrap();
    let delivered = rx.receive_frame(retx_f0);
    assert!(delivered.is_some());
    assert_eq!(delivered.unwrap(), &p_wrap[..]);

    assert_eq!(rx.ack_base, 1);
    assert_eq!(rx.ack_map, 0);
    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert_eq!(tx.in_flight.len(), 0);
}

// ============================================================================
// SUITE 2: PLCP NOISE REJECTION, GOLAY DECODE ERRORS, & INVALID MCS FIELDS
// ============================================================================

#[test]
fn challenge_continuous_gaussian_noise_rejection() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    let noise_sigmas = [0.05f32, 0.20, 0.50, 1.20, 2.50];

    for &sigma in &noise_sigmas {
        let mut rng = ChallengeRng::new(0xFEEDFACE + (sigma * 1000.0) as u64);
        let engine = unsafe { vradm_create(&config) };

        let total_samples = 160_000;
        let mut chunk = [0i16; 160];

        let mut samples_fed = 0;
        while samples_fed < total_samples {
            for s in chunk.iter_mut() {
                let n = rng.next_gaussian() * sigma;
                *s = (n * 32767.0).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
            unsafe {
                vradm_process_audio(engine, chunk.as_ptr(), 160);
            }
            samples_fed += 160;
        }

        let mut rx_buf = [0u8; 512];
        let poll_res = unsafe { vradm_poll_ip_packet(engine, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
        assert_eq!(poll_res, 0, "Noise must not cause false packet delivery");

        let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        unsafe { vradm_get_telemetry(engine, &mut telem) };
        assert_eq!(telem.frames_received, 0);
        assert_eq!(telem.crc_failures, 0);

        unsafe { vradm_destroy(engine) };
    }
}

#[test]
fn challenge_corrupted_golay_words_immediate_idle_search_reset() {
    let mut rx = PhyReceiver::new();

    let preamble = synthesize_barker_preamble();
    let guard_inter = [0.0f32; 80];

    // Codeword 1: 4 bit errors
    let mut valid_bits = encode_plcp_header_bits(2, 2, 0, 10, 0xAB);
    valid_bits[0] = !valid_bits[0];
    valid_bits[1] = !valid_bits[1];
    valid_bits[2] = !valid_bits[2];
    valid_bits[3] = !valid_bits[3];

    let corrupted_fsk = synthesize_2fsk_header(&valid_bits);
    let guard_post = [0.0f32; 80];

    let mut samples_f32 = Vec::new();
    samples_f32.extend_from_slice(&preamble);
    samples_f32.extend_from_slice(&guard_inter);
    samples_f32.extend_from_slice(&corrupted_fsk);
    samples_f32.extend_from_slice(&guard_post);

    let samples_i16: Vec<i16> = samples_f32.iter().map(|&s| (s * 32767.0) as i16).collect();

    let mut offset = 0;
    while offset < samples_i16.len() {
        let end = (offset + 160).min(samples_i16.len());
        rx.ingest_samples(&samples_i16[offset..end]);
        let mut frames = [CanonicalDataFrame::new(); 8];
        let n = rx.process(&mut frames);
        assert_eq!(n, 0);
        offset = end;
    }

    assert_eq!(
        rx.state,
        PhyRxState::IdleSearch,
        "Receiver must reset to IdleSearch on Golay decode failure"
    );

    // Codeword 2: 4 bit errors
    let mut valid_bits2 = encode_plcp_header_bits(2, 2, 0, 10, 0xAB);
    valid_bits2[24] = !valid_bits2[24];
    valid_bits2[25] = !valid_bits2[25];
    valid_bits2[26] = !valid_bits2[26];
    valid_bits2[27] = !valid_bits2[27];

    let corrupted_fsk2 = synthesize_2fsk_header(&valid_bits2);
    let mut samples_f32_2 = Vec::new();
    samples_f32_2.extend_from_slice(&preamble);
    samples_f32_2.extend_from_slice(&guard_inter);
    samples_f32_2.extend_from_slice(&corrupted_fsk2);
    samples_f32_2.extend_from_slice(&guard_post);

    let samples_i16_2: Vec<i16> = samples_f32_2.iter().map(|&s| (s * 32767.0) as i16).collect();
    offset = 0;
    while offset < samples_i16_2.len() {
        let end = (offset + 160).min(samples_i16_2.len());
        rx.ingest_samples(&samples_i16_2[offset..end]);
        let mut frames = [CanonicalDataFrame::new(); 8];
        let n = rx.process(&mut frames);
        assert_eq!(n, 0);
        offset = end;
    }

    assert_eq!(
        rx.state,
        PhyRxState::IdleSearch,
        "Receiver must reset to IdleSearch on Codeword 2 Golay failure"
    );
}

#[test]
fn challenge_invalid_mcs_fields_immediate_reset() {
    let preamble = synthesize_barker_preamble();
    let guard_inter = [0.0f32; 80];
    let guard_post = [0.0f32; 80];

    // Tested invalid MCS values that do not alias: 0, 1, 5
    let invalid_mcs_values = [0u8, 1, 5];

    for &bad_mcs in &invalid_mcs_values {
        let mut rx = PhyReceiver::new();

        let bits = encode_plcp_header_bits(bad_mcs, bad_mcs, 0, 1, 0x11);
        let fsk = synthesize_2fsk_header(&bits);

        let mut samples_f32 = Vec::new();
        samples_f32.extend_from_slice(&preamble);
        samples_f32.extend_from_slice(&guard_inter);
        samples_f32.extend_from_slice(&fsk);
        samples_f32.extend_from_slice(&guard_post);

        let samples_i16: Vec<i16> = samples_f32.iter().map(|&s| (s * 32767.0) as i16).collect();

        let mut offset = 0;
        while offset < samples_i16.len() {
            let end = (offset + 160).min(samples_i16.len());
            rx.ingest_samples(&samples_i16[offset..end]);
            let mut frames = [CanonicalDataFrame::new(); 8];
            let n = rx.process(&mut frames);
            assert_eq!(n, 0, "Invalid MCS {} must not decode frames", bad_mcs);
            offset = end;
        }

        assert_eq!(
            rx.state,
            PhyRxState::IdleSearch,
            "Receiver must reset to IdleSearch when cur_mcs={}",
            bad_mcs
        );
    }
}

#[test]
fn challenge_defect_plcp_cur_mcs6_and_7_aliasing_stalls_receiver() {
    // EMPIRICAL CHALLENGE: When cur_mcs=6 or cur_mcs=7 is transmitted with nominal 80-sample guard,
    // testing offset 0 (0-sample guard for MCS 3) reads an 80-sample shifted 2-FSK stream.
    // In both cases, the 1-bit shifted stream decodes via Golay to cur_mcs=3 with avg_conf > 0.90!
    // Because the receiver tests offset 0 first, it erroneously concludes it is MCS 3,
    // and checks if sample_count < 4120. With 4000 nominal PLCP samples,
    // it hits `break` and stalls in `PlcpHeader` forever without testing offset 80!
    for &bad_mcs in &[6u8, 7u8] {
        let preamble = synthesize_barker_preamble();
        let guard_inter = [0.0f32; 80];
        let guard_post = [0.0f32; 80];

        let bits = encode_plcp_header_bits(bad_mcs, bad_mcs, 0, 1, 0x11);
        let fsk = synthesize_2fsk_header(&bits);

        let mut samples_f32 = Vec::new();
        samples_f32.extend_from_slice(&preamble);
        samples_f32.extend_from_slice(&guard_inter);
        samples_f32.extend_from_slice(&fsk);
        samples_f32.extend_from_slice(&guard_post);

        // Verify 1-bit shifted decode at offset 0:
        let plcp_buf = &samples_f32[520..];
        let mut bits_off0 = [false; 48];
        let mut conf0 = 0.0f32;
        for (b, bit) in bits_off0.iter_mut().enumerate() {
            let offset = b * 80;
            let bit_samples: &[f32; 80] = (&plcp_buf[offset..offset + 80]).try_into().unwrap();
            let (demod_bit, c) = demodulate_2fsk_bit(bit_samples);
            *bit = demod_bit;
            conf0 += c;
        }
        let decode0 = decode_plcp_header(&bits_off0).unwrap();
        assert_eq!(
            decode0.0, 3,
            "DEFECT CONFIRMED: 1-bit shifted MCS {} header aliases to cur_mcs=3!",
            bad_mcs
        );
        assert!(conf0 / 48.0 > 0.90, "Aliased confidence is high (> 0.90)");

        let samples_i16: Vec<i16> = samples_f32.iter().map(|&s| (s * 32767.0) as i16).collect();
        let mut rx = PhyReceiver::new();
        let mut offset = 0;
        while offset < samples_i16.len() {
            let end = (offset + 160).min(samples_i16.len());
            rx.ingest_samples(&samples_i16[offset..end]);
            let mut frames = [CanonicalDataFrame::new(); 8];
            let _ = rx.process(&mut frames);
            offset = end;
        }

        assert_eq!(
            rx.state,
            PhyRxState::IdleSearch,
            "Receiver must reset to IdleSearch when cur_mcs={}",
            bad_mcs
        );
    }
}

// ============================================================================
// SUITE 3: MCS 3 ACELP GUARD TIMING (EXACT 4,640 SAMPLES)
// ============================================================================

#[test]
fn challenge_mcs3_guard_timing_exact_waveform_decomposition() {
    let mut tx = PhyTransmitter::new(3);

    for n_frames in [1, 2, 4, 7] {
        let mut frames = Vec::with_capacity(n_frames);
        for i in 0..n_frames {
            let mut f = CanonicalDataFrame::new();
            f.ctrl = if i == n_frames - 1 { 0x0A } else { 0x02 };
            f.seq = (i + 1) as u8;
            f.payload_len = 4;
            f.payload[..4].copy_from_slice(b"mcs3");
            frames.push(f);
        }

        let burst = tx.modulate_burst(3, &frames, 10);
        let expected_total = 4640 + n_frames * 1320;

        assert_eq!(
            burst.len(),
            expected_total,
            "MCS 3 with {} frames must have exact length {}",
            n_frames,
            expected_total
        );

        // 1. Barker preamble (samples 0..520): non-zero energy
        let barker_energy: f32 = burst[..520].iter().map(|&s| (s as f32) * (s as f32)).sum();
        assert!(barker_energy > 0.0);

        // 2. 2-FSK header (samples 520..4360, 3840 samples): non-zero energy
        let fsk_energy: f32 = burst[520..4360].iter().map(|&s| (s as f32) * (s as f32)).sum();
        assert!(fsk_energy > 0.0);

        // 3. Post-beacon guard silence (samples 4360..4640, EXACTLY 280 samples): MUST BE 0
        for i in 4360..4640 {
            assert_eq!(
                burst[i], 0,
                "Sample {} in MCS 3 post-beacon guard silence must be exact 0",
                i
            );
        }

        // 4. Payload (sample 4640..end): non-zero energy
        let payload_energy: f32 = burst[4640..].iter().map(|&s| (s as f32) * (s as f32)).sum();
        assert!(payload_energy > 0.0);
    }
}

#[test]
fn challenge_mcs3_acelp_and_audiosocket_phase_locking() {
    let plcp_samples = 4640;
    let acelp_subframe_samples = 40; // 5.0 ms @ 8 kHz
    let audiosocket_frame_samples = 160; // 20.0 ms @ 8 kHz

    assert_eq!(plcp_samples % acelp_subframe_samples, 0);
    assert_eq!(plcp_samples / acelp_subframe_samples, 116);

    assert_eq!(plcp_samples % audiosocket_frame_samples, 0);
    assert_eq!(plcp_samples / audiosocket_frame_samples, 29);

    // Nominal PLCP does NOT align evenly with 160
    let nominal_plcp = 4520;
    assert_eq!(nominal_plcp % audiosocket_frame_samples, 40);

    // Closed-loop receiver phase-locking in 160-sample chunks starting at sample 0
    let mut f0 = CanonicalDataFrame::new();
    f0.ctrl = 0x0A;
    f0.seq = 1;
    f0.payload_len = 10;
    f0.payload[..10].copy_from_slice(b"acelp_lock");

    let mut tx = PhyTransmitter::new(3);
    let burst = tx.modulate_burst(3, &[f0], 42);

    let mut rx = PhyReceiver::new();
    let mut decoded = Vec::new();

    let mut offset = 0;
    while offset < burst.len() {
        let end = (offset + 160).min(burst.len());
        rx.ingest_samples(&burst[offset..end]);
        let frames = rx.process_frames();
        decoded.extend(frames);
        offset = end;
    }

    assert_eq!(decoded.len(), 1, "Expected 1 decoded frame at MCS 3");
    assert_eq!(decoded[0], f0);
}

#[test]
fn challenge_defect_barker_detector_rising_flank_early_trigger() {
    // EMPIRICAL CHALLENGE: BarkerDetector correlation peak detector
    // checks `if corr.abs() > 0.60 { found_peak = Some(i); break; }`
    // Because it checks absolute value and breaks on the first sample above 0.60,
    // it triggers on the negative sidelobe at offset -3 (`corr = -0.7489`)!
    let detector = BarkerDetector::new();
    let preamble = synthesize_barker_preamble();

    let mut buffer = vec![0.0f32; 40 + 520 + 100];
    buffer[40..40 + 520].copy_from_slice(&preamble);

    let mut first_match = None;
    let mut max_corr = 0.0f32;
    let mut max_idx = 0;

    for i in 30..=50 {
        let (corr, energy) = detector.correlate(&buffer[i..i + 520]);
        if corr.abs() > 0.60 && energy > 1.0 && first_match.is_none() {
            first_match = Some(i);
        }
        if corr.abs() > max_corr {
            max_corr = corr.abs();
            max_idx = i;
        }
    }

    assert_eq!(max_idx, 40, "Actual peak is at offset 40 (corr = 1.0000)");
    assert_eq!(
        first_match.unwrap(),
        37,
        "DEFECT CONFIRMED: Sidelobe triggers early at offset 37 (corr = -0.7489) due to abs() check!"
    );
}

// ============================================================================
// SUITE 4: BURST TAIL DRAIN WITH PARTIAL CHUNKS (< 160 SAMPLES) & SILENCE PADDING
// ============================================================================

#[test]
fn challenge_burst_tail_drain_partial_chunks_and_silence_padding() {
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

    // Enqueue 1-frame packet: 4,520 (PLCP) + 5,200 (frame) = 9,720 samples
    let test_packet = b"tail_drain_partial_chunk_test";
    unsafe { vradm_write_ip_packet(engine, test_packet.as_ptr(), test_packet.len() as u32) };

    // 9,720 samples / 160 = 60 full chunks (9,600 samples) + 1 tail remainder of 120 samples
    let mut chunk = [0i16; 160];
    for chunk_idx in 0..60 {
        let n = unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };
        assert_eq!(n, 160, "Chunk {} should be full 160 samples", chunk_idx);
        let energy: i64 = chunk.iter().map(|&s| (s as i64) * (s as i64)).sum();
        assert!(energy > 0, "Chunk {} should contain audio data", chunk_idx);
    }

    // Chunk 61: 120 samples remain in tx_ring.
    let n = unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };
    assert_eq!(n, 160, "Tail chunk call should return full buffer size 160");

    let tail_energy: i64 = chunk[..120].iter().map(|&s| (s as i64) * (s as i64)).sum();
    assert!(tail_energy > 0, "Tail chunk first 120 samples must contain audio data");

    for i in 120..160 {
        assert_eq!(
            chunk[i], 0,
            "Sample {} in tail remainder must be padded with silence (0)",
            i
        );
    }

    // 100 subsequent iterations must return pure silence, and frames_transmitted MUST NOT increase!
    for subsequent_idx in 0..100 {
        let m = unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };
        assert_eq!(m, 160);
        let energy: i64 = chunk.iter().map(|&s| (s as i64) * (s as i64)).sum();
        assert_eq!(
            energy, 0,
            "Subsequent chunk {} after tail drain must be pure silence",
            subsequent_idx
        );
    }

    let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut telem) };
    assert_eq!(
        telem.frames_transmitted, 1,
        "Engine must NEVER synthesize a 2nd burst prematurely while waiting for peer turn!"
    );

    unsafe { vradm_destroy(engine) };
}

#[test]
fn challenge_tail_drain_with_arbitrary_partial_requested_chunk_sizes() {
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

    let test_packet = b"arbitrary_partial_chunk_streaming_verification";
    unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };

    // Stream out using irregular partial chunk sizes:
    let chunk_sizes = [40, 80, 25, 73, 117, 33, 50, 160];
    let mut collected_samples = Vec::new();

    let mut chunk_idx = 0;
    let mut silence_chunks = 0;
    let mut buf = [0i16; 200];

    while silence_chunks < 20 {
        let req_size = chunk_sizes[chunk_idx % chunk_sizes.len()];
        chunk_idx += 1;

        let n = unsafe { vradm_generate_audio(engine_a, buf.as_mut_ptr(), req_size) };
        assert_eq!(n, req_size);

        let energy: i64 = buf[..n as usize].iter().map(|&s| (s as i64) * (s as i64)).sum();
        if energy == 0 {
            silence_chunks += 1;
        } else {
            silence_chunks = 0;
        }

        collected_samples.extend_from_slice(&buf[..n as usize]);
    }

    // Feed all collected samples into engine_b in 160-sample chunks
    let mut offset = 0;
    while offset < collected_samples.len() {
        let end = (offset + 160).min(collected_samples.len());
        unsafe {
            vradm_process_audio(engine_b, collected_samples[offset..end].as_ptr(), (end - offset) as u32);
        }
        offset = end;
    }

    let mut rx_buf = [0u8; 128];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(
        poll_res,
        test_packet.len() as i32,
        "Failed to decode packet generated via irregular partial chunks"
    );
    assert_eq!(&rx_buf[..poll_res as usize], test_packet);

    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}
