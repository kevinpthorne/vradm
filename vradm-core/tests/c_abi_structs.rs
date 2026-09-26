use vradm_core::c_abi::*;

macro_rules! offset_of {
    ($Struct:path, $field:ident) => {{
        let b = core::mem::MaybeUninit::<$Struct>::uninit();
        let b_ptr = b.as_ptr();
        let f_ptr = unsafe { core::ptr::addr_of!((*b_ptr).$field) };
        (f_ptr as usize) - (b_ptr as usize)
    }};
}

#[test]
fn test_vradm_config_t_layout() {
    assert_eq!(core::mem::size_of::<vradm_config_t>(), 32, "vradm_config_t must be 32 bytes");
    assert_eq!(core::mem::align_of::<vradm_config_t>(), 16, "vradm_config_t must be 16-byte aligned");
    assert_eq!(offset_of!(vradm_config_t, psk_key), 16, "psk_key offset must be 16");
    assert_eq!(offset_of!(vradm_config_t, tx_amplitude), 8, "tx_amplitude offset must be 8");
}

#[test]
fn test_vradm_telemetry_t_layout() {
    assert_eq!(core::mem::size_of::<vradm_telemetry_t>(), 40, "vradm_telemetry_t must be 40 bytes");
    assert_eq!(offset_of!(vradm_telemetry_t, security_tamper_detected), 8, "security_tamper_detected offset must be 8");
    assert_eq!(offset_of!(vradm_telemetry_t, sample_slip_accum), 36, "sample_slip_accum offset must be 36");
}

#[test]
fn test_vradm_cmd_t_layout() {
    assert_eq!(core::mem::size_of::<vradm_cmd_t>(), 32, "vradm_cmd_t must be 32 bytes");
    assert_eq!(offset_of!(vradm_cmd_t, param_u32), 8, "param_u32 offset must be 8");
    assert_eq!(offset_of!(vradm_cmd_t, inline_payload), 20, "inline_payload offset must be 20");
}

#[test]
fn test_c_abi_constants() {
    assert_eq!(VRADM_FRAME_SIZE, 64);
    assert_eq!(VRADM_CCF_SIZE, 16);
    assert_eq!(VRADM_MAX_PAYLOAD_SIZE, 38);
    assert_eq!(VRADM_MAX_IP_DATA_SIZE, 37);

    assert_eq!(VRADM_MCS_0, 0);
    assert_eq!(VRADM_MCS_1, 1);
    assert_eq!(VRADM_MCS_2, 2);
    assert_eq!(VRADM_MCS_3, 3);
    assert_eq!(VRADM_MCS_4, 4);

    assert_eq!(VRADM_RATE_8K, 8000);
    assert_eq!(VRADM_RATE_16K, 16000);

    assert_eq!(VRADM_OK, 0);
    assert_eq!(VRADM_ERR_INVALID_ARG, -1);
    assert_eq!(VRADM_ERR_BUFFER_TOO_SMALL, -2);
    assert_eq!(VRADM_ERR_QUEUE_FULL, -3);
    assert_eq!(VRADM_ERR_STATE, -4);
    assert_eq!(VRADM_ERR_VERIFICATION, -5);
}
