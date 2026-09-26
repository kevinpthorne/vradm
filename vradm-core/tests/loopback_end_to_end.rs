use vradm_core::c_abi::*;

#[test]
fn test_closed_loop_ip_packet_single_fragment_loopback() {
    // 1. Initialize Node A and Node B
    let config_a = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };
    let engine_a = unsafe { vradm_create(&config_a) };
    assert!(!engine_a.is_null(), "Failed to create Node A");

    let config_b = config_a;
    let engine_b = unsafe { vradm_create(&config_b) };
    assert!(!engine_b.is_null(), "Failed to create Node B");

    // 2. 20-byte small IP packet (single fragment)
    let mut test_packet = [0u8; 20];
    test_packet[0] = 0x45; // IPv4
    test_packet[9] = 1;    // ICMP
    for i in 10..20 {
        test_packet[i] = (i * 7 + 3) as u8;
    }

    // 3. Write packet to Node A
    let write_res = unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };
    assert_eq!(write_res, VRADM_OK, "write_ip_packet failed");

    // 4. Generate audio chunks (160 samples / 20 ms each)
    let mut audio_channel: Vec<i16> = Vec::new();
    let mut pcm_chunk = [0i16; 160];
    let max_chunks = 150; // Plenty for 1 frame burst (PLCP 4520 + 5200 = 9720 samples ~ 61 chunks)

    for _ in 0..max_chunks {
        let generated = unsafe { vradm_generate_audio(engine_a, pcm_chunk.as_mut_ptr(), 160) };
        if generated > 0 {
            audio_channel.extend_from_slice(&pcm_chunk[..generated as usize]);
        }
        let mut telem_a: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        unsafe { vradm_get_telemetry(engine_a, &mut telem_a) };
        if telem_a.frames_transmitted >= 1 && audio_channel.len() >= 9720 {
            break;
        }
    }

    assert!(audio_channel.len() >= 9720, "Generated {} samples, expected at least 9720", audio_channel.len());

    // 5. Stream audio into Node B in 160-sample chunks
    let mut offset = 0;
    while offset < audio_channel.len() {
        let chunk_size = 160.min(audio_channel.len() - offset);
        unsafe {
            vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    // 6. Poll reassembled packet from Node B
    let mut rx_buf = [0u8; 128];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_res, test_packet.len() as i32, "Poll packet length mismatch");
    assert_eq!(&rx_buf[..poll_res as usize], &test_packet[..], "Reassembled payload corrupted!");

    // 7. Verify Node B telemetry
    let mut telem_b: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine_b, &mut telem_b) };
    assert_eq!(telem_b.frames_received, 1);
    assert_eq!(telem_b.plcp_carrier_locked, 1);

    // 8. Clean destruction
    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}

#[test]
fn test_closed_loop_ip_packet_multi_fragment_mtu256_loopback() {
    // 1. Initialize Node A and Node B
    let config_a = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };
    let engine_a = unsafe { vradm_create(&config_a) };
    assert!(!engine_a.is_null());

    let config_b = config_a;
    let engine_b = unsafe { vradm_create(&config_b) };
    assert!(!engine_b.is_null());

    // 2. Realistic 256-byte MTU IPv4 datagram (7 fragments)
    let mut test_packet = [0u8; 256];
    test_packet[0] = 0x45; // IPv4
    test_packet[9] = 6;    // TCP
    test_packet[12..16].copy_from_slice(&[10, 99, 0, 2]); // Src
    test_packet[16..20].copy_from_slice(&[10, 99, 0, 1]); // Dst
    for i in 20..256 {
        test_packet[i] = ((i * 37 + 13) & 0xFF) as u8;
    }

    // 3. Write packet to Node A
    let write_res = unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), test_packet.len() as u32) };
    assert_eq!(write_res, VRADM_OK);

    // 4. Generate audio burst (PLCP 4520 + 7 * 5200 = 40,920 samples ~ 256 chunks of 160)
    let mut audio_channel: Vec<i16> = Vec::new();
    let mut pcm_chunk = [0i16; 160];
    let max_chunks = 300;

    for _ in 0..max_chunks {
        let generated = unsafe { vradm_generate_audio(engine_a, pcm_chunk.as_mut_ptr(), 160) };
        if generated > 0 {
            audio_channel.extend_from_slice(&pcm_chunk[..generated as usize]);
        }
        let mut telem_a: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        unsafe { vradm_get_telemetry(engine_a, &mut telem_a) };
        if telem_a.frames_transmitted >= 7 && audio_channel.len() >= 40920 {
            break;
        }
    }

    assert!(audio_channel.len() >= 40920, "Generated {} samples, expected at least 40920", audio_channel.len());

    // 5. Stream audio into Node B in 160-sample chunks
    let mut offset = 0;
    while offset < audio_channel.len() {
        let chunk_size = 160.min(audio_channel.len() - offset);
        unsafe {
            vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    // 6. Poll reassembled 256-byte packet from Node B
    let mut rx_buf = [0u8; 512];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_res, test_packet.len() as i32, "Poll packet length mismatch");
    assert_eq!(&rx_buf[..poll_res as usize], &test_packet[..], "Reassembled 256B datagram corrupted!");

    // 7. Verify Node B telemetry
    let mut telem_b: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine_b, &mut telem_b) };
    assert_eq!(telem_b.frames_received, 7, "Expected 7 frames received");
    assert_eq!(telem_b.crc_failures, 0, "Expected 0 CRC failures");

    // 8. Clean destruction
    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}

#[test]
fn test_closed_loop_ip_packet_max_296_bytes_loopback() {
    let config_a = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };
    let engine_a = unsafe { vradm_create(&config_a) };
    let engine_b = unsafe { vradm_create(&config_a) };

    // Maximum datagram size (8 fragments * 37 bytes = 296 bytes)
    let mut test_packet = [0u8; 296];
    test_packet[0] = 0x45;
    for i in 1..296 {
        test_packet[i] = ((i * 13 + 7) & 0xFF) as u8;
    }

    let write_res = unsafe { vradm_write_ip_packet(engine_a, test_packet.as_ptr(), 296) };
    assert_eq!(write_res, VRADM_OK);

    // Generate audio (4520 + 8 * 5200 = 46120 samples)
    let mut audio_channel = Vec::new();
    let mut chunk = [0i16; 160];
    for _ in 0..320 {
        let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
        if n > 0 {
            audio_channel.extend_from_slice(&chunk[..n as usize]);
        }
        let mut telem_a: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        unsafe { vradm_get_telemetry(engine_a, &mut telem_a) };
        if telem_a.frames_transmitted >= 8 && audio_channel.len() >= 46120 {
            break;
        }
    }

    // Stream into Node B
    let mut offset = 0;
    while offset < audio_channel.len() {
        let chunk_size = 160.min(audio_channel.len() - offset);
        unsafe {
            vradm_process_audio(engine_b, audio_channel[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    // Poll reassembled datagram
    let mut rx_buf = [0u8; 512];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_res, 296);
    assert_eq!(&rx_buf[..296], &test_packet[..]);

    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}

#[test]
fn test_closed_loop_piggyback_ack() {
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

    // Node A sends 1-fragment packet
    let packet_a = b"node_a_request_packet_12345";
    unsafe { vradm_write_ip_packet(engine_a, packet_a.as_ptr(), packet_a.len() as u32) };

    let mut audio_a = Vec::new();
    let mut chunk = [0i16; 160];
    for _ in 0..100 {
        let n = unsafe { vradm_generate_audio(engine_a, chunk.as_mut_ptr(), 160) };
        if n > 0 {
            audio_a.extend_from_slice(&chunk[..n as usize]);
        }
        if audio_a.len() >= 9720 {
            break;
        }
    }

    // Stream into Node B
    let mut offset = 0;
    while offset < audio_a.len() {
        let chunk_size = 160.min(audio_a.len() - offset);
        unsafe {
            vradm_process_audio(engine_b, audio_a[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    // Node B receives packet
    let mut rx_buf = [0u8; 128];
    let poll_res = unsafe { vradm_poll_ip_packet(engine_b, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_res, packet_a.len() as i32);

    // Node B writes response packet
    let packet_b = b"node_b_response_ack_piggyback";
    unsafe { vradm_write_ip_packet(engine_b, packet_b.as_ptr(), packet_b.len() as u32) };

    // Node B generates return audio with piggybacked ACK
    let mut audio_b = Vec::new();
    for _ in 0..100 {
        let n = unsafe { vradm_generate_audio(engine_b, chunk.as_mut_ptr(), 160) };
        if n > 0 {
            audio_b.extend_from_slice(&chunk[..n as usize]);
        }
        if audio_b.len() >= 9720 {
            break;
        }
    }

    // Stream return audio into Node A
    offset = 0;
    while offset < audio_b.len() {
        let chunk_size = 160.min(audio_b.len() - offset);
        unsafe {
            vradm_process_audio(engine_a, audio_b[offset..offset + chunk_size].as_ptr(), chunk_size as u32);
        }
        offset += chunk_size;
    }

    // Node A should have received Node B's packet
    let poll_a = unsafe { vradm_poll_ip_packet(engine_a, rx_buf.as_mut_ptr(), rx_buf.len() as u32) };
    assert_eq!(poll_a, packet_b.len() as i32);
    assert_eq!(&rx_buf[..poll_a as usize], packet_b);

    // Node A's in-flight frames should be acknowledged
    let inflight_count = unsafe { (*(*engine_a).arq_tx.get()).in_flight.len() };
    assert_eq!(inflight_count, 0, "All in-flight frames on Node A must be acknowledged");

    unsafe {
        vradm_destroy(engine_a);
        vradm_destroy(engine_b);
    }
}
