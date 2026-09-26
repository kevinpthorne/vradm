use std::ptr;
use std::thread;
use vradm_core::c_abi::*;

#[test]
fn test_telemetry_null_safety() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_1,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());

    let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };

    // vradm_get_telemetry with NULLs
    unsafe { vradm_get_telemetry(ptr::null(), &mut telem) };
    unsafe { vradm_get_telemetry(engine, ptr::null_mut()) };

    // vradm_get_active_mcs with NULL
    assert_eq!(unsafe { vradm_get_active_mcs(ptr::null()) }, VRADM_MCS_0);
    assert_eq!(unsafe { vradm_get_active_mcs(engine) }, VRADM_MCS_1);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_telemetry_seqlock_concurrency() {
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

    let engine_addr = engine as usize;
    let iterations = 10_000;

    // Spawn concurrent reader thread
    let reader_handle = thread::spawn(move || {
        let eng = engine_addr as *const vradm_engine_t;
        let mut t: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        for _ in 0..iterations {
            unsafe { vradm_get_telemetry(eng, &mut t) };
            // Check that frames_transmitted is a valid multiple or consistent
            assert!(t.frames_transmitted <= 200_000);
            assert_eq!(t.frames_received, t.frames_transmitted * 2);
        }
    });

    // Main thread acts as audio thread writing updates
    for i in 1..=iterations {
        unsafe {
            (*engine).telem_seqlock.update(|t| {
                t.frames_transmitted = i as u32;
                t.frames_received = (i * 2) as u32;
            });
        }
    }

    reader_handle.join().expect("Reader thread panicked");

    let mut final_telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut final_telem) };
    assert_eq!(final_telem.frames_transmitted, iterations as u32);
    assert_eq!(final_telem.frames_received, (iterations * 2) as u32);

    unsafe { vradm_destroy(engine) };
}
