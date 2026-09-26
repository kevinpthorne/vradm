use std::ptr;
use vradm_core::c_abi::*;

#[test]
fn test_cmd_null_safety() {
    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 1,
        param_u32: 3,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };

    // Engine is NULL
    let res = unsafe { vradm_submit_cmd(ptr::null_mut(), &cmd) };
    assert_eq!(res, VRADM_ERR_INVALID_ARG);

    // Cmd is NULL
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_0,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let res = unsafe { vradm_submit_cmd(engine, ptr::null()) };
    assert_eq!(res, VRADM_ERR_INVALID_ARG);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_cmd_submit_mcs_request() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_0,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());
    assert_eq!(unsafe { vradm_get_active_mcs(engine) }, 0);

    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 101,
        param_u32: 3, // Request MCS 3
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };

    let res = unsafe { vradm_submit_cmd(engine, &cmd) };
    assert_eq!(res, VRADM_OK);

    // Pump audio thread to process command queue
    let mut dummy_pcm = [0i16; 160];
    unsafe { vradm_generate_audio(engine, dummy_pcm.as_mut_ptr(), 160) };

    // MCS should have transitioned to 3
    assert_eq!(unsafe { vradm_get_active_mcs(engine) }, 3);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_cmd_queue_overflow() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_0,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_NONE,
        cmd_id: 1,
        param_u32: 0,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };

    // Capacity is 32. Fill 32 commands without draining
    for _ in 0..32 {
        let res = unsafe { vradm_submit_cmd(engine, &cmd) };
        assert_eq!(res, VRADM_OK);
    }

    // 33rd command must return VRADM_ERR_QUEUE_FULL (-3)
    let res = unsafe { vradm_submit_cmd(engine, &cmd) };
    assert_eq!(res, VRADM_ERR_QUEUE_FULL);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn queued_reset_preserves_commands_submitted_after_it() {
    let config = vradm_config_t {
        sample_rate: 8000, startup_mcs: 2, auto_rate_adaptation: 0,
        reserved: [0; 2], tx_amplitude: 0.1778, reserved2: [0; 4], psk_key: [0; 16],
    };
    unsafe {
        let engine = vradm_create(&config);
        let mut cmd: vradm_cmd_t = std::mem::zeroed();
        cmd.cmd_type = VRADM_CMD_RESET_SESSION;
        assert_eq!(vradm_submit_cmd(engine, &cmd), VRADM_OK);
        cmd.cmd_type = VRADM_CMD_REQUEST_MCS;
        cmd.param_u32 = 3;
        assert_eq!(vradm_submit_cmd(engine, &cmd), VRADM_OK);
        let mut pcm = [0; 160];
        vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
        let mcs = vradm_get_active_mcs(engine);
        vradm_destroy(engine);
        assert_eq!(mcs, 3, "reset must not discard subsequent host commands");
    }
}

#[test]
fn queued_reset_waits_for_active_burst_to_finish() {
    let config = vradm_config_t {
        sample_rate: 8000, startup_mcs: 2, auto_rate_adaptation: 0,
        reserved: [0; 2], tx_amplitude: 0.1778, reserved2: [0; 4], psk_key: [0; 16],
    };
    unsafe {
        let engine = vradm_create(&config);
        assert_eq!(vradm_write_ip_packet(engine, [0x42; 19].as_ptr(), 19), VRADM_OK);
        let mut pcm = [0; 160];
        vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
        let mut cmd: vradm_cmd_t = std::mem::zeroed();
        cmd.cmd_type = VRADM_CMD_RESET_SESSION;
        assert_eq!(vradm_submit_cmd(engine, &cmd), VRADM_OK);
        cmd.cmd_type = VRADM_CMD_REQUEST_MCS;
        cmd.param_u32 = 3;
        assert_eq!(vradm_submit_cmd(engine, &cmd), VRADM_OK);
        // One MCS2 frame + PLCP = 9720 samples, rounded to 61 callbacks.
        for _ in 0..60 {
            vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
            assert_eq!(vradm_get_active_mcs(engine), 2);
        }
        vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
        assert_eq!(vradm_get_active_mcs(engine), 3);
        assert!(pcm.iter().all(|&sample| sample == 0));
        vradm_destroy(engine);
    }
}
