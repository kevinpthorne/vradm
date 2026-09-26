use std::ptr;
use vradm_core::c_abi::*;

#[test]
fn test_all_13_c_abi_null_safety() {
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
    assert!(!engine.is_null());

    let mut dummy_buf = [0u8; 512];
    let mut dummy_pcm = [0i16; 160];
    let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    let dummy_cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 1,
        param_u32: 3,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };
    let mut u32_val1 = 0u32;
    let mut u32_val2 = 0u32;
    let mut hash_buf = [0u8; 32];

    // 1. vradm_create
    assert!(unsafe { vradm_create(ptr::null()) }.is_null());

    // 2. vradm_destroy
    unsafe { vradm_destroy(ptr::null_mut()) };

    // 3. vradm_reset
    unsafe { vradm_reset(ptr::null_mut()) };

    // 4. vradm_submit_cmd
    assert_eq!(unsafe { vradm_submit_cmd(ptr::null_mut(), &dummy_cmd) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_submit_cmd(engine, ptr::null()) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_submit_cmd(ptr::null_mut(), ptr::null()) }, VRADM_ERR_INVALID_ARG);

    // 5. vradm_process_audio
    unsafe { vradm_process_audio(ptr::null_mut(), dummy_pcm.as_ptr(), 160) };
    unsafe { vradm_process_audio(engine, ptr::null(), 160) };
    unsafe { vradm_process_audio(engine, dummy_pcm.as_ptr(), 0) };
    unsafe { vradm_process_audio(ptr::null_mut(), ptr::null(), 0) };

    // 6. vradm_generate_audio
    assert_eq!(unsafe { vradm_generate_audio(ptr::null_mut(), dummy_pcm.as_mut_ptr(), 160) }, 0);
    assert_eq!(unsafe { vradm_generate_audio(engine, ptr::null_mut(), 160) }, 0);
    assert_eq!(unsafe { vradm_generate_audio(engine, dummy_pcm.as_mut_ptr(), 0) }, 0);
    assert_eq!(unsafe { vradm_generate_audio(ptr::null_mut(), ptr::null_mut(), 0) }, 0);

    // 7. vradm_write_ip_packet
    assert_eq!(unsafe { vradm_write_ip_packet(ptr::null_mut(), dummy_buf.as_ptr(), 100) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, ptr::null(), 100) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, dummy_buf.as_ptr(), 0) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(ptr::null_mut(), ptr::null(), 0) }, VRADM_ERR_INVALID_ARG);

    // 8. vradm_poll_ip_packet
    assert_eq!(unsafe { vradm_poll_ip_packet(ptr::null_mut(), dummy_buf.as_mut_ptr(), 100) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_poll_ip_packet(engine, ptr::null_mut(), 100) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_poll_ip_packet(engine, dummy_buf.as_mut_ptr(), 0) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_poll_ip_packet(ptr::null_mut(), ptr::null_mut(), 0) }, VRADM_ERR_INVALID_ARG);

    // 9. vradm_sotp_stage_tx_payload
    assert_eq!(unsafe { vradm_sotp_stage_tx_payload(ptr::null_mut(), dummy_buf.as_ptr(), 100, 1.2, &mut u32_val1) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_stage_tx_payload(engine, ptr::null(), 100, 1.2, &mut u32_val1) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_stage_tx_payload(engine, dummy_buf.as_ptr(), 0, 1.2, &mut u32_val1) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_stage_tx_payload(engine, dummy_buf.as_ptr(), 100, 0.99, &mut u32_val1) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_stage_tx_payload(engine, dummy_buf.as_ptr(), 100, 1.2, ptr::null_mut()) }, VRADM_ERR_INVALID_ARG);

    // 10. vradm_sotp_rx_poll
    assert_eq!(unsafe { vradm_sotp_rx_poll(ptr::null_mut(), &mut u32_val1, &mut u32_val2) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_rx_poll(engine, ptr::null_mut(), &mut u32_val2) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_rx_poll(engine, &mut u32_val1, ptr::null_mut()) }, VRADM_ERR_INVALID_ARG);

    // 11. vradm_sotp_rx_fetch
    assert_eq!(unsafe { vradm_sotp_rx_fetch(ptr::null_mut(), dummy_buf.as_mut_ptr(), 100, hash_buf.as_mut_ptr()) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_rx_fetch(engine, ptr::null_mut(), 100, hash_buf.as_mut_ptr()) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_rx_fetch(engine, dummy_buf.as_mut_ptr(), 0, hash_buf.as_mut_ptr()) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_sotp_rx_fetch(engine, dummy_buf.as_mut_ptr(), 100, ptr::null_mut()) }, VRADM_ERR_INVALID_ARG);

    // 12. vradm_get_telemetry
    unsafe { vradm_get_telemetry(ptr::null(), &mut telem) };
    unsafe { vradm_get_telemetry(engine, ptr::null_mut()) };
    unsafe { vradm_get_telemetry(ptr::null(), ptr::null_mut()) };

    // 13. vradm_get_active_mcs
    assert_eq!(unsafe { vradm_get_active_mcs(ptr::null()) }, VRADM_MCS_0);
    assert_eq!(unsafe { vradm_get_active_mcs(engine) }, VRADM_MCS_2);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_oversized_and_boundary_ip_packets() {
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
    assert!(!engine.is_null());

    let buf = [0x55u8; 4096];

    // Zero length: rejected
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 0) }, VRADM_ERR_INVALID_ARG);

    // Min valid length: 1 byte
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 1) }, VRADM_OK);

    // Standard MTU: 256 bytes
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 256) }, VRADM_OK);

    // Max valid length: 296 bytes (8 frags * 37 bytes)
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 296) }, VRADM_OK);

    // 1 byte over max: 297 bytes: MUST be rejected
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 297) }, VRADM_ERR_INVALID_ARG);

    // Large lengths
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 500) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 1500) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), 4096) }, VRADM_ERR_INVALID_ARG);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, buf.as_ptr(), u32::MAX) }, VRADM_ERR_INVALID_ARG);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_poll_ip_buffer_too_small() {
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
    assert!(!engine.is_null());

    // Stage an rx packet directly into engine's rx_packet_queue
    let eng_ref = unsafe { &mut *engine };
    let mut slot = vradm_core::engine::PacketSlot::default();
    slot.len = 200;
    slot.data[..200].fill(0x77);
    let push_res = eng_ref.rx_packet_queue.push(slot);
    assert!(push_res.is_ok());

    // Attempt to poll with buffer smaller than 200 bytes (e.g. 100 bytes)
    let mut small_buf = [0u8; 100];
    let poll_res = unsafe { vradm_poll_ip_packet(engine, small_buf.as_mut_ptr(), 100) };
    assert_eq!(poll_res, VRADM_ERR_BUFFER_TOO_SMALL, "Buffer smaller than packet must return ERR_BUFFER_TOO_SMALL");

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_rapid_reset_destroy_sequences() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    // 1. 2000 rapid create/destroy cycles
    for _ in 0..2000 {
        let engine = unsafe { vradm_create(&config) };
        assert!(!engine.is_null());
        unsafe { vradm_destroy(engine) };
    }

    // 2. 1000 rapid reset cycles with active queue entries
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let packet = [0xBB; 128];
    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 1,
        param_u32: 3,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };

    for _ in 0..1000 {
        // Enqueue some packets and commands
        for _ in 0..5 {
            unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32) };
            unsafe { vradm_submit_cmd(engine, &cmd) };
        }
        // Rapid reset
        unsafe { vradm_reset(engine) };

        // Verify queues are completely drained
        let mut out = [0u8; 256];
        assert_eq!(unsafe { vradm_poll_ip_packet(engine, out.as_mut_ptr(), 256) }, 0);
        let eng_ref = unsafe { &mut *engine };
        assert!(eng_ref.tx_packet_queue.is_empty());
        assert!(eng_ref.rx_packet_queue.is_empty());
        assert!(eng_ref.cmd_queue.is_empty());
    }

    // 3. 500 complete lifecycle cycles with audio generation
    let mut pcm = [0i16; 160];
    for _ in 0..500 {
        let eng = unsafe { vradm_create(&config) };
        assert!(!eng.is_null());

        // Write packet
        assert_eq!(unsafe { vradm_write_ip_packet(eng, packet.as_ptr(), packet.len() as u32) }, VRADM_OK);
        // Generate audio
        let gen = unsafe { vradm_generate_audio(eng, pcm.as_mut_ptr(), 160) };
        assert_eq!(gen, 160);

        // Reset
        unsafe { vradm_reset(eng) };

        // Write another packet after reset
        assert_eq!(unsafe { vradm_write_ip_packet(eng, packet.as_ptr(), packet.len() as u32) }, VRADM_OK);

        // Destroy
        unsafe { vradm_destroy(eng) };
    }

    unsafe { vradm_destroy(engine) };
}
