use std::ptr;
use vradm_core::c_abi::*;
use vradm_core::engine::blake3_224;

#[test]
fn test_sotp_stage_and_null_safety() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let payload = b"hello_sotp_broadcast_data_payload_test";
    let mut obj_id = 0u32;

    // NULL engine
    assert_eq!(
        unsafe { vradm_sotp_stage_tx_payload(ptr::null_mut(), payload.as_ptr(), payload.len() as u32, 1.5, &mut obj_id) },
        VRADM_ERR_INVALID_ARG
    );

    // NULL payload
    assert_eq!(
        unsafe { vradm_sotp_stage_tx_payload(engine, ptr::null(), payload.len() as u32, 1.5, &mut obj_id) },
        VRADM_ERR_INVALID_ARG
    );

    // NULL out_object_id
    assert_eq!(
        unsafe { vradm_sotp_stage_tx_payload(engine, payload.as_ptr(), payload.len() as u32, 1.5, ptr::null_mut()) },
        VRADM_ERR_INVALID_ARG
    );

    // Redundancy factor < 1.0
    assert_eq!(
        unsafe { vradm_sotp_stage_tx_payload(engine, payload.as_ptr(), payload.len() as u32, 0.9, &mut obj_id) },
        VRADM_ERR_INVALID_ARG
    );

    // Valid stage
    let res = unsafe {
        vradm_sotp_stage_tx_payload(engine, payload.as_ptr(), payload.len() as u32, 1.5, &mut obj_id)
    };
    assert_eq!(res, VRADM_OK);
    assert_ne!(obj_id, 0);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_sotp_rx_poll_and_fetch_lifecycle() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let mut collected = 0u32;
    let mut required = 0u32;

    // Initially IDLE
    let state = unsafe { vradm_sotp_rx_poll(engine, &mut collected, &mut required) };
    assert_eq!(state, VRADM_SOTP_STATE_IDLE);

    // Attempting fetch when IDLE returns VRADM_ERR_STATE
    let mut fetch_buf = [0u8; 128];
    let mut hash_buf = [0u8; 28];
    let fetch_res = unsafe {
        vradm_sotp_rx_fetch(engine, fetch_buf.as_mut_ptr(), fetch_buf.len() as u32, hash_buf.as_mut_ptr())
    };
    assert_eq!(fetch_res, VRADM_ERR_STATE);

    // Simulate verified object ready on engine
    let test_data = b"raptorq_verified_object_payload_content_1234567890";
    let expected_hash = blake3_224(test_data);
    unsafe {
        (*engine).stage_rx_object_for_test(42, test_data);
    }

    // Now poll should return READY
    let state = unsafe { vradm_sotp_rx_poll(engine, &mut collected, &mut required) };
    assert_eq!(state, VRADM_SOTP_STATE_READY);
    assert!(required > 0);
    assert_eq!(collected, required);

    // Test buffer too small
    let mut small_buf = [0u8; 10];
    let too_small_res = unsafe {
        vradm_sotp_rx_fetch(engine, small_buf.as_mut_ptr(), small_buf.len() as u32, hash_buf.as_mut_ptr())
    };
    assert_eq!(too_small_res, VRADM_ERR_BUFFER_TOO_SMALL);

    // Valid fetch
    let fetch_len = unsafe {
        vradm_sotp_rx_fetch(engine, fetch_buf.as_mut_ptr(), fetch_buf.len() as u32, hash_buf.as_mut_ptr())
    };
    assert_eq!(fetch_len, test_data.len() as i32);
    assert_eq!(&fetch_buf[..fetch_len as usize], test_data);
    assert_eq!(hash_buf, expected_hash);

    // After fetch, state transitions back to IDLE
    let state_after = unsafe { vradm_sotp_rx_poll(engine, &mut collected, &mut required) };
    assert_eq!(state_after, VRADM_SOTP_STATE_IDLE);

    unsafe { vradm_destroy(engine) };
}
