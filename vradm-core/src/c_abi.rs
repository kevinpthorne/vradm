//! Raw C ABI. All handles and buffers must remain valid for each call.
//! Per engine, one host owner serializes commands, packet I/O, and SOTP calls;
//! one audio owner serializes both PCM callbacks. These two owners may run
//! concurrently. Reset/destruction require all users to be quiescent. Telemetry
//! readers may run concurrently while the engine remains alive.
//! Rust callers can enforce owner separation with `vradm_engine::split` instead.

#![allow(non_camel_case_types)]

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq)]
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
#[derive(Clone, Copy, Debug, PartialEq)]
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
#[derive(Clone, Copy, Debug, PartialEq)]
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

// Status & Error Codes
pub const VRADM_OK: i32 = 0;
pub const VRADM_ERR_INVALID_ARG: i32 = -1;
pub const VRADM_ERR_BUFFER_TOO_SMALL: i32 = -2;
pub const VRADM_ERR_QUEUE_FULL: i32 = -3;
pub const VRADM_ERR_STATE: i32 = -4;
pub const VRADM_ERR_VERIFICATION: i32 = -5;

// SOTP Reception States
pub const VRADM_SOTP_STATE_IDLE: i32 = 0;
pub const VRADM_SOTP_STATE_ACCUMULATING: i32 = 1;
pub const VRADM_SOTP_STATE_READY: i32 = 2;
pub const VRADM_SOTP_STATE_ERROR: i32 = 3;

// Type Aliases matching vradm_core.h
pub type vradm_mcs_t = u8;
pub type vradm_rate_t = u32;
pub type vradm_cmd_type_t = u32;
pub type vradm_engine_t = crate::engine::vradm_engine;

// ============================================================================
// 13 C-ABI Function Exports (§10 vradm_core.h)
// ============================================================================

/// Lifecycle: Allocates and initializes engine on the Rust heap
#[no_mangle]
pub unsafe extern "C" fn vradm_create(config: *const vradm_config_t) -> *mut vradm_engine_t {
    if config.is_null() {
        return core::ptr::null_mut();
    }
    let cfg = &*config;
    if (cfg.sample_rate != VRADM_RATE_8K && cfg.sample_rate != VRADM_RATE_16K)
        || cfg.startup_mcs > VRADM_MCS_4
        || !cfg.tx_amplitude.is_finite()
        || cfg.tx_amplitude < 0.0
        || cfg.tx_amplitude > 1.0
    {
        return core::ptr::null_mut();
    }
    let engine = Box::new(crate::engine::vradm_engine::new(*cfg));
    Box::into_raw(engine)
}

/// Lifecycle: Deallocates engine under Quiescence Invariant
#[no_mangle]
pub unsafe extern "C" fn vradm_destroy(engine: *mut vradm_engine_t) {
    if !engine.is_null() {
        drop(Box::from_raw(engine));
    }
}

/// Lifecycle: Resets internal link state under Quiescence Invariant
#[no_mangle]
pub unsafe extern "C" fn vradm_reset(engine: *mut vradm_engine_t) {
    if let Some(eng) = engine.as_ref() {
        eng.reset();
    }
}

/// Commands: single-host-owner asynchronous command submission.
/// A successful RESET_SESSION submission starts a new local queue generation
/// and clears host-owned SOTP staging on this calling (non-audio) thread.
/// Audio-owned link state resets at the next transmit-burst boundary. Commands
/// and packets submitted afterwards are preserved. Queue-full rejection has no
/// reset side effects. This does not negotiate a peer session or authenticate
/// late PCM; the host must coordinate the remote session separately.
///
/// # Safety
/// The handle and command must be valid. Exactly one host thread may submit
/// commands, write/poll packets, or manipulate SOTP state for this engine;
/// exactly one audio thread owns both audio callbacks. Direct reset/destruction
/// requires quiescence as described in SPEC §10.
#[no_mangle]
pub unsafe extern "C" fn vradm_submit_cmd(engine: *mut vradm_engine_t, cmd: *const vradm_cmd_t) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    let c = match cmd.as_ref() {
        Some(cmd_ref) => cmd_ref,
        None => return VRADM_ERR_INVALID_ARG,
    };
    eng.submit_cmd(c)
}

/// Real-Time Audio: Ingests linear PCM samples (zero-alloc, non-blocking)
#[no_mangle]
pub unsafe extern "C" fn vradm_process_audio(engine: *mut vradm_engine_t, in_samples: *const i16, count: u32) {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return,
    };
    if in_samples.is_null() || count == 0 {
        return;
    }
    let samples = core::slice::from_raw_parts(in_samples, count as usize);
    eng.process_audio(samples);
}

/// Real-Time Audio: Synthesizes linear PCM samples (zero-alloc, non-blocking)
#[no_mangle]
pub unsafe extern "C" fn vradm_generate_audio(engine: *mut vradm_engine_t, out_samples: *mut i16, max_count: u32) -> u32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return 0,
    };
    if out_samples.is_null() || max_count == 0 {
        return 0;
    }
    let out = core::slice::from_raw_parts_mut(out_samples, max_count as usize);
    eng.generate_audio(out)
}

/// L3 Interface: Slices and enqueues IP packet into transmit ring
#[no_mangle]
pub unsafe extern "C" fn vradm_write_ip_packet(engine: *mut vradm_engine_t, packet: *const u8, len: u32) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    if packet.is_null() {
        return VRADM_ERR_INVALID_ARG;
    }
    let pkt = core::slice::from_raw_parts(packet, len as usize);
    eng.write_ip_packet(pkt)
}

/// L3 Interface: Polls reassembled IP packet from receive ring
#[no_mangle]
pub unsafe extern "C" fn vradm_poll_ip_packet(engine: *mut vradm_engine_t, out_packet: *mut u8, max_len: u32) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    if out_packet.is_null() || max_len == 0 {
        return VRADM_ERR_INVALID_ARG;
    }
    let out = core::slice::from_raw_parts_mut(out_packet, max_len as usize);
    eng.poll_ip_packet(out)
}

/// SOTP: Stages transmission payload synchronously into engine memory
#[no_mangle]
pub unsafe extern "C" fn vradm_sotp_stage_tx_payload(
    engine: *mut vradm_engine_t,
    payload: *const u8,
    len: u32,
    redundancy_factor: f32,
    out_object_id: *mut u32,
) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    if payload.is_null() || out_object_id.is_null() || len == 0 || redundancy_factor < 1.0 {
        return VRADM_ERR_INVALID_ARG;
    }
    let p = core::slice::from_raw_parts(payload, len as usize);
    eng.sotp_stage_tx_payload(p, redundancy_factor, &mut *out_object_id)
}

/// SOTP: Polls object reception status and symbol counts
#[no_mangle]
pub unsafe extern "C" fn vradm_sotp_rx_poll(
    engine: *mut vradm_engine_t,
    out_collected_symbols: *mut u32,
    out_required_symbols: *mut u32,
) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    if out_collected_symbols.is_null() || out_required_symbols.is_null() {
        return VRADM_ERR_INVALID_ARG;
    }
    eng.sotp_rx_poll(&mut *out_collected_symbols, &mut *out_required_symbols)
}

/// SOTP: Fetches verified reconstructed object and BLAKE3-224 hash
#[no_mangle]
pub unsafe extern "C" fn vradm_sotp_rx_fetch(
    engine: *mut vradm_engine_t,
    out_buf: *mut u8,
    max_len: u32,
    out_hash: *mut u8,
) -> i32 {
    let eng = match engine.as_ref() {
        Some(e) => e,
        None => return VRADM_ERR_INVALID_ARG,
    };
    if out_buf.is_null() || out_hash.is_null() || max_len == 0 {
        return VRADM_ERR_INVALID_ARG;
    }
    let buf = core::slice::from_raw_parts_mut(out_buf, max_len as usize);
    let hash = core::slice::from_raw_parts_mut(out_hash, 28);
    eng.sotp_rx_fetch(buf, hash)
}

/// Telemetry: Reads consistent double-buffered snapshot via seqlock
#[no_mangle]
pub unsafe extern "C" fn vradm_get_telemetry(engine: *const vradm_engine_t, out_telem: *mut vradm_telemetry_t) {
    if let (Some(eng), Some(out)) = (engine.as_ref(), out_telem.as_mut()) {
        eng.get_telemetry(out);
    }
}

/// Telemetry: Zero-overhead atomic load for TCP-PEP window clamping
#[no_mangle]
pub unsafe extern "C" fn vradm_get_active_mcs(engine: *const vradm_engine_t) -> u8 {
    if let Some(eng) = engine.as_ref() {
        eng.get_active_mcs()
    } else {
        VRADM_MCS_0
    }
}

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
