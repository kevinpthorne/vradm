use std::ptr;
use vradm_core::c_abi::*;

#[test]
fn test_write_ip_null_safety_and_bounds() {
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

    let packet = [0x45u8; 100];

    // Engine NULL
    assert_eq!(
        unsafe { vradm_write_ip_packet(ptr::null_mut(), packet.as_ptr(), 100) },
        VRADM_ERR_INVALID_ARG
    );

    // Packet NULL
    assert_eq!(
        unsafe { vradm_write_ip_packet(engine, ptr::null(), 100) },
        VRADM_ERR_INVALID_ARG
    );

    // Len == 0
    assert_eq!(
        unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), 0) },
        VRADM_ERR_INVALID_ARG
    );

    // Len > 296 (MAX_IP_PACKET_LEN: 8 fragments * 37 bytes)
    let oversized = [0x45u8; 297];
    assert_eq!(
        unsafe { vradm_write_ip_packet(engine, oversized.as_ptr(), 297) },
        VRADM_ERR_INVALID_ARG
    );

    // Valid lengths
    assert_eq!(unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), 1) }, VRADM_OK);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), 37) }, VRADM_OK);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), 256) }, VRADM_OK);
    assert_eq!(unsafe { vradm_write_ip_packet(engine, oversized.as_ptr(), 296) }, VRADM_OK);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_poll_ip_null_safety_and_empty() {
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

    let mut buf = [0u8; 256];

    // Engine NULL
    assert_eq!(
        unsafe { vradm_poll_ip_packet(ptr::null_mut(), buf.as_mut_ptr(), 256) },
        VRADM_ERR_INVALID_ARG
    );

    // Out buf NULL
    assert_eq!(
        unsafe { vradm_poll_ip_packet(engine, ptr::null_mut(), 256) },
        VRADM_ERR_INVALID_ARG
    );

    // Max len == 0
    assert_eq!(
        unsafe { vradm_poll_ip_packet(engine, buf.as_mut_ptr(), 0) },
        VRADM_ERR_INVALID_ARG
    );

    // Empty queue returns 0
    assert_eq!(
        unsafe { vradm_poll_ip_packet(engine, buf.as_mut_ptr(), 256) },
        0
    );

    unsafe { vradm_destroy(engine) };
}
