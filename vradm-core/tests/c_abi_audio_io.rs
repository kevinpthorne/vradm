use std::ptr;
use vradm_core::c_abi::*;

#[test]
fn test_audio_io_null_safety() {
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

    let in_samples = [0i16; 160];
    let mut out_samples = [0i16; 160];

    // vradm_process_audio NULL safety
    unsafe { vradm_process_audio(ptr::null_mut(), in_samples.as_ptr(), 160) };
    unsafe { vradm_process_audio(engine, ptr::null(), 160) };
    unsafe { vradm_process_audio(engine, in_samples.as_ptr(), 0) };

    // vradm_generate_audio NULL safety
    assert_eq!(unsafe { vradm_generate_audio(ptr::null_mut(), out_samples.as_mut_ptr(), 160) }, 0);
    assert_eq!(unsafe { vradm_generate_audio(engine, ptr::null_mut(), 160) }, 0);
    assert_eq!(unsafe { vradm_generate_audio(engine, out_samples.as_mut_ptr(), 0) }, 0);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_generate_audio_idle_silence() {
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

    // When idle with no packets, generate_audio should produce 0 (silence)
    let mut out_samples = [12345i16; 320];
    let generated = unsafe { vradm_generate_audio(engine, out_samples.as_mut_ptr(), 320) };
    assert_eq!(generated, 320);

    // Verify samples were zero-filled (silence)
    for &s in out_samples.iter() {
        assert_eq!(s, 0, "Idle audio must be silence");
    }

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_generate_audio_peak_limiter_bounds() {
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

    // Write a packet so audio burst is synthesized
    let packet = [0xA5u8; 100];
    unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), 100) };

    let mut chunk = [0i16; 160];
    let max_peak_expected = (0.50f32 * 32767.0f32).ceil() as i16;

    // Drain several thousand samples
    for _ in 0..50 {
        let n = unsafe { vradm_generate_audio(engine, chunk.as_mut_ptr(), 160) };
        assert_eq!(n, 160);
        for &s in chunk.iter() {
            assert!(
                s.abs() <= max_peak_expected + 1,
                "Sample {} exceeds peak ceiling {}",
                s,
                max_peak_expected
            );
        }
    }

    unsafe { vradm_destroy(engine) };
}

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct TrackingAllocator;

thread_local! {
    static THREAD_TRACKING: Cell<bool> = const { Cell::new(false) };
    static THREAD_ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
    static THREAD_DEALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = THREAD_TRACKING.try_with(|tracking| {
            if tracking.get() {
                let _ = THREAD_ALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = THREAD_TRACKING.try_with(|tracking| {
            if tracking.get() {
                let _ = THREAD_DEALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        System.dealloc(ptr, layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = THREAD_TRACKING.try_with(|tracking| {
            if tracking.get() {
                let _ = THREAD_ALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = THREAD_TRACKING.try_with(|tracking| {
            if tracking.get() {
                let _ = THREAD_ALLOC_COUNT.try_with(|c| c.set(c.get() + 1));
            }
        });
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator;

#[test]
fn test_zero_allocation_in_audio_generate_and_process() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    };

    // 1. Creation and initialization allocates once on host thread
    let engine_tx = unsafe { vradm_create(&config) };
    let engine_rx = unsafe { vradm_create(&config) };
    assert!(!engine_tx.is_null());
    assert!(!engine_rx.is_null());

    // 2. Enqueue an IP packet to transmit
    let packet = [0x42u8; 100];
    let written = unsafe { vradm_write_ip_packet(engine_tx, packet.as_ptr(), packet.len() as u32) };
    assert_eq!(written, VRADM_OK);

    // 3. Begin tracking allocations on this real-time audio thread
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));

    let mut chunk = [0i16; 160];

    // Generate and process audio across an entire burst cycle
    for _ in 0..70 {
        let gen = unsafe { vradm_generate_audio(engine_tx, chunk.as_mut_ptr(), 160) };
        assert_eq!(gen, 160);

        // Process audio in real-time callback on receiver
        unsafe { vradm_process_audio(engine_rx, chunk.as_ptr(), 160) };
    }

    // Stop tracking
    THREAD_TRACKING.with(|t| t.set(false));

    let allocs = THREAD_ALLOC_COUNT.with(|c| c.get());
    let deallocs = THREAD_DEALLOC_COUNT.with(|c| c.get());

    assert_eq!(
        allocs, 0,
        "ZERO-ALLOCATION INVARIANT VIOLATION: {} heap allocations occurred in real-time audio callbacks!",
        allocs
    );
    assert_eq!(
        deallocs, 0,
        "ZERO-ALLOCATION INVARIANT VIOLATION: {} heap deallocations occurred in real-time audio callbacks!",
        deallocs
    );

    // 4. Teardown
    unsafe {
        vradm_destroy(engine_tx);
        vradm_destroy(engine_rx);
    }
}

#[test]
fn full_receive_queue_backpressure_does_not_allocate_on_audio_thread() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K, startup_mcs: VRADM_MCS_3,
        auto_rate_adaptation: 0, reserved: [0; 2], tx_amplitude: 0.1334,
        reserved2: [0; 4], psk_key: [0x5a; 16],
    };
    let tx = unsafe { vradm_create(&config) };
    let rx = unsafe { vradm_create(&config) };
    assert!(!tx.is_null() && !rx.is_null());
    let receiver = unsafe { &mut *rx };
    for _ in 0..64 {
        let mut slot = vradm_core::engine::PacketSlot::default();
        slot.len = 1;
        assert!(receiver.rx_packet_queue.push(slot).is_ok());
    }
    let packet = [0x62; 256];
    assert_eq!(unsafe { vradm_write_ip_packet(tx, packet.as_ptr(), 256) }, VRADM_OK);
    let mut data = [0; 160];
    let mut feedback = [0; 160];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for _ in 0..500 {
        unsafe {
            vradm_generate_audio(tx, data.as_mut_ptr(), 160);
            vradm_process_audio(rx, data.as_ptr(), 160);
            vradm_generate_audio(rx, feedback.as_mut_ptr(), 160);
            vradm_process_audio(tx, feedback.as_ptr(), 160);
        }
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
    let receiver = unsafe { &*rx };
    let receiver_arq = unsafe { &*receiver.arq_rx.get() };
    assert_eq!(receiver_arq.ack_base, 255, "blocked data must not be acknowledged");
    assert_eq!(receiver.rx_packet_queue.len(), 64);
    unsafe { vradm_destroy(tx); vradm_destroy(rx); }
}

#[test]
fn gmd_low_confidence_ranking_is_allocation_free() {
    let frame = vradm_core::arq::IpPacketSlicer::slice(&[0x45; 30], false, false, 0).unwrap()[0];
    let encoded = frame.encode();
    // Exercise the long ranking path even for a clean codeword.
    let confidences = [0.1; 64];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    let result = vradm_core::phy::decode_gmd_canonical_frame(&encoded, &confidences);
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(result, Ok(frame));
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn ordered_gap_release_does_not_allocate() {
    use vradm_core::arq::{ArqReceiver, IpPacketSlicer};
    use vradm_core::framing::CanonicalDataFrame;
    let mut receiver = ArqReceiver::new();
    let mut packets = [CanonicalDataFrame::new(); 8];
    for i in 0..8u8 {
        let mut sliced = [CanonicalDataFrame::new(); 8];
        IpPacketSlicer::slice_into(&[i; 19], false, false, i, &mut sliced).unwrap();
        packets[i as usize] = sliced[0];
    }
    let mut output = [0; 296];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for frame in packets[1..].iter().rev() { receiver.ingest_frame(frame); }
    assert_eq!(receiver.poll_packet_into(&mut output), None);
    receiver.ingest_frame(&packets[0]);
    for i in 0..8u8 {
        assert_eq!(receiver.poll_packet_into(&mut output), Some((19, false)));
        assert_eq!(&output[..19], &[i; 19]);
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn queued_reset_keeps_object_deallocation_off_audio_thread() {
    let config = vradm_config_t {
        sample_rate: 8000, startup_mcs: 2, auto_rate_adaptation: 0,
        reserved: [0; 2], tx_amplitude: 0.1778, reserved2: [0; 4], psk_key: [0; 16],
    };
    unsafe {
        let engine = vradm_create(&config);
        let payload = [0x58; 2048];
        let mut object_id = 0;
        assert_eq!(vradm_sotp_stage_tx_payload(engine, payload.as_ptr(), 2048, 1.2, &mut object_id), VRADM_OK);
        (*engine).stage_rx_object_for_test(1, &payload);
        let mut reset: vradm_cmd_t = std::mem::zeroed();
        reset.cmd_type = VRADM_CMD_RESET_SESSION;
        assert_eq!(vradm_submit_cmd(engine, &reset), VRADM_OK);
        THREAD_ALLOC_COUNT.with(|c| c.set(0));
        THREAD_DEALLOC_COUNT.with(|c| c.set(0));
        THREAD_TRACKING.with(|t| t.set(true));
        let mut pcm = [0; 160];
        vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
        vradm_process_audio(engine, pcm.as_ptr(), 160);
        THREAD_TRACKING.with(|t| t.set(false));
        assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
        assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
        let mut collected = 99;
        let mut required = 99;
        assert_eq!(vradm_sotp_rx_poll(engine, &mut collected, &mut required), VRADM_SOTP_STATE_IDLE);
        assert_eq!((collected, required), (0, 0));
        vradm_destroy(engine);
    }
}

#[test]
fn concurrent_host_staging_and_queued_resets_keep_audio_allocation_free() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    use std::thread;
    use std::time::{Duration, Instant};
    let config = vradm_config_t {
        sample_rate: 8000, startup_mcs: 2, auto_rate_adaptation: 0,
        reserved: [0; 2], tx_amplitude: 0.1778, reserved2: [0; 4], psk_key: [0; 16],
    };
    let engine = unsafe { vradm_create(&config) };
    assert!(!engine.is_null());
    let address = engine as usize;
    let done = Arc::new(AtomicBool::new(false));
    let audio_done = done.clone();
    let audio = thread::spawn(move || {
        let engine = address as *mut vradm_engine_t;
        let mut pcm = [0; 160];
        let mut calls = 0;
        THREAD_ALLOC_COUNT.with(|c| c.set(0));
        THREAD_DEALLOC_COUNT.with(|c| c.set(0));
        THREAD_TRACKING.with(|t| t.set(true));
        while !audio_done.load(Ordering::Acquire) {
            unsafe {
                vradm_generate_audio(engine, pcm.as_mut_ptr(), 160);
                vradm_process_audio(engine, pcm.as_ptr(), 160);
            }
            calls += 1;
            thread::yield_now();
        }
        THREAD_TRACKING.with(|t| t.set(false));
        (calls, THREAD_ALLOC_COUNT.with(Cell::get), THREAD_DEALLOC_COUNT.with(Cell::get))
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut resets = 0;
    while resets < 200 && Instant::now() < deadline {
        let payload = [0x42; 128];
        let mut object_id = 0;
        let mut cmd: vradm_cmd_t = unsafe { std::mem::zeroed() };
        cmd.cmd_type = VRADM_CMD_RESET_SESSION;
        unsafe {
            assert_eq!(vradm_sotp_stage_tx_payload(engine, payload.as_ptr(), 128, 1.2, &mut object_id), VRADM_OK);
            let result = vradm_submit_cmd(engine, &cmd);
            assert!(result == VRADM_OK || result == VRADM_ERR_QUEUE_FULL);
            if result == VRADM_OK { resets += 1; }
            let result = vradm_write_ip_packet(engine, payload.as_ptr(), 19);
            assert!(result == VRADM_OK || result == VRADM_ERR_QUEUE_FULL);
            let mut output = [0; 296];
            assert!(vradm_poll_ip_packet(engine, output.as_mut_ptr(), 296) >= 0);
        }
        thread::yield_now();
    }
    done.store(true, Ordering::Release);
    let (calls, allocs, frees) = audio.join().unwrap();
    unsafe { vradm_destroy(engine) };
    assert_eq!(resets, 200, "host/audio progress stalled");
    assert!(calls > 0);
    assert_eq!((allocs, frees), (0, 0));
}

#[test]
fn security_primitives_and_multichunk_hash_are_allocation_free() {
    use vradm_core::framing::CompactControlFrame;
    use vradm_core::security::*;
    let payload = [0x57; 4097];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    let mut pending = PendingBootstrap::new([1; 16], [2; 16]);
    let request = VerifiedRequest::decode(&pending.request().unwrap(), &[1; 16]).unwrap();
    let (reply, responder) = request.accept(&[1; 16], [3; 16]);
    let initiator = pending.finish(&reply).unwrap();
    let mut tx = ControlTx::new(initiator);
    let mut rx = ControlRx::new(responder.clone(), 0);
    let counter = rx.verify_beacon(tx.beacon(3, 2, 1).unwrap(), 0).unwrap();
    let wire = responder.sign_ccf(counter, CompactControlFrame::new()).unwrap();
    let accepted = rx.verify_ccf(wire, &[], counter, 0).is_ok();
    let digest = vradm_core::engine::blake3_224(&payload);
    THREAD_TRACKING.with(|t| t.set(false));
    assert!(accepted);
    assert_ne!(digest, [0; 28]);
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn bootstrap_payload_callbacks_do_not_allocate() {
    use vradm_core::bootstrap_phy::*;
    let wire = vradm_core::security::PendingBootstrap::new([1; 16], [2; 16]).request().unwrap();
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    let mut tx = BootstrapFskTransmitter::new(wire);
    let mut rx = BootstrapFskReceiver::at_frame_start();
    let mut out = [0; 173];
    let mut result = None;
    while tx.remaining_samples() > 0 {
        let count = tx.render(&mut out);
        let progress = rx.push(&out[..count]);
        if progress.frame.is_some() { result = progress.frame; }
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(result, Some(Ok(wire)));
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn bootstrap_acquisition_callbacks_do_not_allocate() {
    use vradm_core::bootstrap_phy::*;
    let wire = vradm_core::security::PendingBootstrap::new([1; 16], [2; 16]).request().unwrap();
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    let mut tx = BarkerBootstrapTransmitter::new(wire);
    let mut rx = BarkerBootstrapReceiver::new();
    rx.push(&[0; 317]);
    let mut out = [0; 173];
    let mut result = None;
    while tx.remaining_samples() > 0 {
        let count = tx.render(&mut out);
        let progress = rx.push(&out[..count]);
        if progress.frame.is_some() { result = progress.frame; }
    }
    rx.reset();
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(result, Some(Ok(wire)));
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn authenticated_session_install_and_pcm_callbacks_do_not_allocate() {
    use vradm_core::{engine::vradm_engine, session::*};
    struct Fixed(u8);
    impl NonceSource for Fixed {
        fn nonce(&mut self) -> Result<[u8; 16], SessionError> { Ok([self.0; 16]) }
    }
    fn transmitted(event: SessionEvent) -> [u8; 32] {
        match event { SessionEvent::Transmit(wire) => wire, _ => panic!("expected transmit") }
    }
    let mut a = SessionManager::<8>::new([1; 16], 0);
    let mut b = SessionManager::<8>::new([1; 16], 0);
    let req = transmitted(a.begin(0, BootstrapMode::Fsk100, &mut Fixed(2)).unwrap());
    let reply = transmitted(b.receive(&req, 0, BootstrapMode::Fsk100, &mut Fixed(3)).unwrap());
    a.receive(&reply, 0, BootstrapMode::Fsk100, &mut Fixed(4)).unwrap();
    b.verify_peer_beacon(a.beacon(3, 3, 0, 0).unwrap(), 0).unwrap();
    let config = vradm_config_t { sample_rate: VRADM_RATE_8K, startup_mcs: 3,
        auto_rate_adaptation: 0, reserved: [0; 2], tx_amplitude: 0.1334,
        reserved2: [0; 4], psk_key: [1; 16] };
    let mut ae = vradm_engine::new_authenticated(config).unwrap();
    let mut be = vradm_engine::new_authenticated(config).unwrap();
    let (mut ah, mut aa) = ae.split(); let (mut bh, mut ba) = be.split();
    assert!(ah.install_session(a.take_established(0).unwrap()).is_ok());
    assert!(bh.install_session(b.take_established(0).unwrap()).is_ok());
    ah.write_ip_packet(&[0x41; 19]); bh.write_ip_packet(&[0x42; 19]);
    let mut out = [0; 160]; let mut back = [0; 160];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for _ in 0..300 {
        aa.generate_audio(&mut out); ba.process_audio(&out);
        ba.generate_audio(&mut back); aa.process_audio(&back);
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(ah.poll_ip_packet(&mut [0; 296]), 19);
    assert_eq!(bh.poll_ip_packet(&mut [0; 296]), 19);
    assert_eq!(THREAD_ALLOC_COUNT.with(Cell::get), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(Cell::get), 0);
}

#[test]
fn handshake_bridge_callbacks_allocate_and_free_nothing() {
    use vradm_core::handshake_bridge::HandshakeBridge;
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    let mut out = [0; 113];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for _ in 0..100 {
        host.push_playback(&[1; 160]).unwrap();
        audio.render(&mut out);
        audio.render(&mut out);
        audio.capture(&[2; 511]);
        while host.pop_capture().is_some() {}
        host.seal_playback().unwrap();
        audio.render(&mut out);
        let fence = audio.playback_fence().unwrap();
        audio.acknowledge_played(fence);
        host.reset();
        audio.capture_discontinuity();
        audio.render(&mut out);
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}

#[test]
fn idle_confirmation_retries_allocate_and_free_nothing() {
    use vradm_core::{engine::vradm_engine, handshake::HandshakeCoordinator, session::{NonceSource, SessionError}};
    struct Fixed(u8);
    impl NonceSource for Fixed {
        fn nonce(&mut self) -> Result<[u8; 16], SessionError> { Ok([self.0; 16]) }
    }
    let mut a = HandshakeCoordinator::<8>::new([1; 16], 3, 0).unwrap();
    let mut b = HandshakeCoordinator::<8>::new([1; 16], 3, 0).unwrap();
    let mut handshake_pcm = vec![0; 21000];
    a.begin(0, &mut Fixed(2)).unwrap();
    a.render_pcm(&mut handshake_pcm, 0).unwrap();
    b.process_pcm(&handshake_pcm, 2625, &mut Fixed(3)).unwrap();
    b.render_pcm(&mut handshake_pcm, 2625).unwrap();
    a.process_pcm(&handshake_pcm, 5250, &mut Fixed(2)).unwrap();
    a.render_pcm(&mut handshake_pcm, 5250).unwrap();
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K, startup_mcs: VRADM_MCS_3, auto_rate_adaptation: 0,
        reserved: [0; 2], tx_amplitude: 0.1334, reserved2: [0; 4], psk_key: [1; 16],
    };
    let mut engine = vradm_engine::new_authenticated(config).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host.install_session(a.take_established(6500).unwrap()).is_ok());
    let mut pcm = [0; 160];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for _ in 0..1500 { audio.generate_audio(&mut pcm); audio.process_audio(&[0; 160]); }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
    let mut telemetry = unsafe { core::mem::zeroed() };
    host.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.frames_transmitted, 3);
}

#[test]
fn authenticated_control_transactions_allocate_and_free_nothing() {
    use vradm_core::{framing::CompactControlFrame, security::*};
    let keys = SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]);
    let mut tx = ControlTx::new(keys.clone());
    let mut rx = ControlRx::new(keys.clone(), 0);
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for counter in 0..300u16 {
        let request = ControlRequest { current_mcs: 2, target_mcs: 3, tx_power: 0,
            command: ControlCommand::McsCommitAck, yield_turn: true, deadline_ms: 1000 };
        tx.begin_control(request, 0).unwrap();
        let wire = keys.sign_ccf(counter, CompactControlFrame { ccf_ctrl: 0xba,
            ack_base: counter as u8, ack_map: 3, ccf_mac: 0 }).unwrap();
        assert!(rx.verify_control_response(&mut tx, wire, &[], 0).unwrap().apply_semantics);
        assert!(!rx.verify_control_response(&mut tx, wire, &[], 0).unwrap().apply_semantics);
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}

#[test]
fn mcs_negotiation_commit_retry_and_cooldown_allocate_and_free_nothing() {
    use vradm_core::{framing::CompactControlFrame, mcs_control::*, security::*};
    let keys = SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]);
    let mut tx = ControlTx::new(keys.clone());
    let mut rx = ControlRx::new(keys.clone(), 0);
    let mut policy = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    policy.request_upshift(3, 0, true, 100, 0).unwrap();
    let wire = keys.sign_ccf(0, CompactControlFrame { ccf_ctrl: 0xba,
        ack_base: 255, ack_map: 0, ccf_mac: 0 }).unwrap();
    policy.receive(wire, &[], 1).unwrap();
    policy.complete_commit(1).unwrap();
    policy.emergency_downshift(2, 2).unwrap();
    policy.request_upshift(3, 0, true, 100, 2).unwrap();
    policy.poll(102).unwrap();
    policy.poll(202).unwrap();
    assert_eq!(policy.poll(302), Ok(McsControlEvent::Aborted));
    policy.data_beacon(0, 302).unwrap();
    policy.request_upshift(3, 0, true, 100, 10302).unwrap();
    policy.cancel(10302).unwrap();
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}

#[test]
fn aligned_ccf_pcm_render_decode_and_erasure_recovery_allocate_nothing() {
    use vradm_core::{ccf_phy::*, framing::CompactControlFrame};
    let wire = CompactControlFrame { ccf_ctrl: 0xba, ack_base: 255, ack_map: 3, ccf_mac: 17 }.encode();
    let mut tx = CcfPitchTransmitter::new(wire);
    let mut rx = CcfPitchReceiver::at_frame_start();
    let mut pcm = [0;160];
    let mut frames = 0;
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for erase in [false, true] {
        tx.reset(wire);
        rx.reset_to_frame_start();
        for block in 0..80 {
            tx.render(&mut pcm);
            if erase && block < 5 { pcm.fill(0); }
            if let Some(frame) = rx.push(&pcm).frame {
                let frame = frame.unwrap();
                assert_eq!(CompactControlFrame::decode(frame.codeword(), frame.erasures()).unwrap().ccf_mac, 17);
                frames += 1;
            }
        }
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(frames, 2);
}

#[test]
fn ccf_turn_start_render_cancel_and_reuse_allocate_nothing() {
    use vradm_core::{ccf_turn::*, framing::CompactControlFrame};
    let wire = CompactControlFrame { ccf_ctrl: 0xba, ack_base: 7, ack_map: 3, ccf_mac: 17 }.encode();
    let mut tx = CcfTurnTransmitter::new();
    let mut pcm = [0;511];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    tx.start(wire).unwrap();
    tx.render(&mut pcm);
    assert_eq!(tx.start(wire), Err(CcfTurnError::Busy));
    tx.cancel();
    assert_eq!(tx.render(&mut pcm), 0);
    for _ in 0..3 {
        tx.start(wire).unwrap();
        while tx.remaining_samples() > 0 { tx.render(&mut pcm); }
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}

#[test]
fn ccf_turn_receive_success_failure_and_reset_allocate_nothing() {
    use vradm_core::{ccf_phy::CCF_PCM_SAMPLES, ccf_turn::*, framing::CompactControlFrame};
    let wire = CompactControlFrame { ccf_ctrl: 0xba, ack_base: 7, ack_map: 3, ccf_mac: 17 }.encode();
    let mut tx = CcfTurnTransmitter::new();
    tx.start(wire).unwrap();
    let mut pcm = vec![0;CCF_TURN_SAMPLES];
    tx.render(&mut pcm);
    let mut rx = CcfTurnReceiver::at_frame_start();
    let mut events = 0;
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for fail in [false, true] {
        rx.reset_to_frame_start();
        if fail { pcm[CCF_PCM_SAMPLES..CCF_PCM_SAMPLES+EOT_SAMPLES].fill(0); }
        for chunk in pcm.chunks(79) {
            if let Some(frame) = rx.push(chunk).frame {
                assert_eq!(frame.is_err(), fail);
                events += 1;
            }
        }
    }
    rx.reset_to_frame_start();
    rx.push(&pcm[..CCF_PCM_SAMPLES+17]);
    rx.reset_to_frame_start();
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(events, 2);
}

#[test]
fn verified_mcs_response_admission_and_signing_allocate_nothing() {
    use vradm_core::security::*;
    let keys = SessionKeys::derive(&[1;16], &[2;16], &[3;16]);
    let mut tx = ControlTx::new(keys.clone());
    let mut rx = ControlRx::new(keys, 0);
    let local = McsReplyParameters { channel_metric: 0.85, ack_base: 255, ack_map: 3, yield_turn: true };
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    for counter in 0..300u16 {
        let request = tx.beacon(2,3,0).unwrap();
        let reply = rx.prepare_mcs_commit(request, local, 0).unwrap();
        assert_eq!(reply.request_counter(), counter);
        assert_eq!(reply.first_sequence(), 0);
        assert_eq!(rx.prepare_mcs_commit(request, local, 0).unwrap_err(), McsReplyError::Security(SecurityError::Replay));
    }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}

#[test]
fn amplitude_command_and_live_burst_conditioning_allocate_nothing() {
    use vradm_core::engine::vradm_engine;
    let config = vradm_config_t { sample_rate: 8000, startup_mcs: 2, auto_rate_adaptation: 0,
        reserved: [0;2], tx_amplitude: 0.04, reserved2: [0;4], psk_key: [0;16] };
    let mut engine = vradm_engine::new(config);
    let (mut host, mut audio) = engine.split();
    let cmd = vradm_cmd_t { cmd_type: VRADM_CMD_SET_TX_PARAMS, cmd_id: 0,
        param_u32: 0, param_i32: 0, param_f32: 0.02, inline_payload: [0;12] };
    let mut pcm = [0;160];
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));
    assert_eq!(host.submit_cmd(&cmd), VRADM_OK);
    host.write_ip_packet(&[42;19]);
    for _ in 0..70 { audio.generate_audio(&mut pcm); }
    THREAD_TRACKING.with(|t| t.set(false));
    assert_eq!(THREAD_ALLOC_COUNT.with(|c| c.get()), 0);
    assert_eq!(THREAD_DEALLOC_COUNT.with(|c| c.get()), 0);
}
