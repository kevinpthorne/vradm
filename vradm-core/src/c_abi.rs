#[repr(C, align(16))]
pub struct vradm_config_t {
    pub sample_rate: u32,
    pub startup_mcs: u8,
    pub auto_rate_adaptation: u8,
    pub reserved: [u8; 2],
    pub tx_amplitude: f32,
    pub reserved2: [u8; 4],
    pub psk_key: [u8; 16],
}

#[repr(C)]
pub struct vradm_telemetry_t {
    pub estimated_snr_db: f32,
    pub active_tx_mcs: u8,
    pub active_rx_mcs: u8,
    pub plcp_carrier_locked: u8,
    pub reserved: u8,
    pub security_tamper_detected: u32,
    pub frames_transmitted: u32,
    pub frames_received: u32,
    pub rs_corrected_bytes: u32,
    pub rs_corrected_erasures: u32,
    pub crc_failures: u32,
    pub channel_metric_score: f32,
    pub sample_slip_accum: i32,
}

#[repr(C)]
pub struct vradm_cmd_t {
    pub cmd_type: u32,
    pub cmd_id: u32,
    pub param_u32: u32,
    pub param_i32: i32,
    pub param_f32: f32,
    pub inline_payload: [u8; 12],
}

pub const VRADM_FRAME_SIZE: usize = 64;
pub const VRADM_CCF_SIZE: usize = 16;
pub const VRADM_MAX_PAYLOAD_SIZE: usize = 38;
pub const VRADM_MAX_IP_DATA_SIZE: usize = 37;

pub const VRADM_MCS_0: u8 = 0;
pub const VRADM_MCS_1: u8 = 1;
pub const VRADM_MCS_2: u8 = 2;
pub const VRADM_MCS_3: u8 = 3;
pub const VRADM_MCS_4: u8 = 4;

pub const VRADM_RATE_8K: u32 = 8000;
pub const VRADM_RATE_16K: u32 = 16000;

pub const VRADM_CMD_NONE: u32 = 0;
pub const VRADM_CMD_START_SOTP: u32 = 1;
pub const VRADM_CMD_STOP_SOTP: u32 = 2;
pub const VRADM_CMD_REQUEST_MCS: u32 = 3;
pub const VRADM_CMD_RESET_SESSION: u32 = 4;
pub const VRADM_CMD_SET_TX_PARAMS: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! offset_of {
        ($Struct:path, $field:ident) => {{
            let b = core::mem::MaybeUninit::<$Struct>::uninit();
            let b_ptr = b.as_ptr();
            let f_ptr = unsafe { core::ptr::addr_of!((*b_ptr).$field) };
            (f_ptr as usize) - (b_ptr as usize)
        }};
    }

    #[test]
    fn test_c_abi_struct_sizes() {
        assert_eq!(core::mem::size_of::<vradm_config_t>(), 32, "vradm_config_t must be 32 bytes");
        assert_eq!(core::mem::align_of::<vradm_config_t>(), 16, "vradm_config_t must be 16-byte aligned");
        assert_eq!(core::mem::size_of::<vradm_telemetry_t>(), 40, "vradm_telemetry_t must be 40 bytes");
        assert_eq!(core::mem::size_of::<vradm_cmd_t>(), 32, "vradm_cmd_t must be 32 bytes");
    }

    #[test]
    fn test_c_abi_struct_offsets() {
        assert_eq!(offset_of!(vradm_config_t, psk_key), 16, "psk_key offset must be 16");
        assert_eq!(offset_of!(vradm_telemetry_t, security_tamper_detected), 8, "security_tamper_detected offset must be 8");
        assert_eq!(offset_of!(vradm_telemetry_t, sample_slip_accum), 36, "sample_slip_accum offset must be 36");
        assert_eq!(offset_of!(vradm_cmd_t, param_u32), 8, "param_u32 offset must be 8");
        assert_eq!(offset_of!(vradm_cmd_t, inline_payload), 20, "inline_payload offset must be 20");
    }
}
