use std::ptr;
use vradm_core::c_abi::*;

#[test]
fn test_create_destroy_clean() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0xAA; 16],
    };

    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null(), "vradm_create returned NULL for valid config");

    let mcs = unsafe { vradm_get_active_mcs(engine) };
    assert_eq!(mcs, VRADM_MCS_2);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_create_invalid_args() {
    // 1. NULL config pointer
    let engine = unsafe { vradm_create(ptr::null()) };
    assert!(engine.is_null(), "vradm_create must return NULL for null config");

    // 2. Invalid sample rate (e.g. 44100 Hz)
    let mut config = vradm_config_t {
        sample_rate: 44100,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(engine.is_null(), "vradm_create must reject invalid sample rate");

    // 3. Invalid MCS (> 4)
    config.sample_rate = VRADM_RATE_8K;
    config.startup_mcs = 5;
    let engine = unsafe { vradm_create(&config) };
    assert!(engine.is_null(), "vradm_create must reject invalid startup MCS");

    // 4. Invalid tx_amplitude (< 0.0 or > 1.0)
    config.startup_mcs = VRADM_MCS_2;
    config.tx_amplitude = 1.5;
    let engine = unsafe { vradm_create(&config) };
    assert!(engine.is_null(), "vradm_create must reject tx_amplitude > 1.0");

    config.tx_amplitude = -0.1;
    let engine = unsafe { vradm_create(&config) };
    assert!(engine.is_null(), "vradm_create must reject negative tx_amplitude");
}

#[test]
fn test_destroy_null_safety() {
    // Calling vradm_destroy on NULL must not segfault or panic
    unsafe { vradm_destroy(ptr::null_mut()) };
}

#[test]
fn test_reset_null_safety_and_state_flush() {
    // NULL safety
    unsafe { vradm_reset(ptr::null_mut()) };

    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_1,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x55; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    // Write a packet to enqueue state
    let pkt = b"test_packet_for_reset";
    let w_res = unsafe { vradm_write_ip_packet(engine, pkt.as_ptr(), pkt.len() as u32) };
    assert_eq!(w_res, VRADM_OK);

    // Call reset
    unsafe { vradm_reset(engine) };

    // Verify polling returns 0 (queue emptied)
    let mut out_buf = [0u8; 64];
    let p_res = unsafe { vradm_poll_ip_packet(engine, out_buf.as_mut_ptr(), out_buf.len() as u32) };
    assert_eq!(p_res, 0);

    // Verify MCS restored to startup_mcs
    let mcs = unsafe { vradm_get_active_mcs(engine) };
    assert_eq!(mcs, VRADM_MCS_1);

    unsafe { vradm_destroy(engine) };
}
