use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use vradm_core::arq::{ArqReceiver, FragmentHeader};
use vradm_core::c_abi::*;
use vradm_core::framing::CanonicalDataFrame;

#[test]
fn test_concurrent_host_write_and_audio_generate() {
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

    let engine_addr = engine as usize;
    let total_packets = 200u32;
    let packets_enqueued = Arc::new(AtomicU32::new(0));
    let enqueued_clone = Arc::clone(&packets_enqueued);
    let done = Arc::new(AtomicBool::new(false));
    let done_host = Arc::clone(&done);
    let done_telem = Arc::clone(&done);

    // 1. Host Network Thread: rapidly writes IP packets
    let host_thread = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut pkt = [0u8; 128];
        for seq in 0..total_packets {
            pkt[0] = 0x45; // IPv4
            pkt[1] = (seq >> 8) as u8;
            pkt[2] = (seq & 0xFF) as u8;
            let len = 30 + (seq % 100) as usize; // Variable lengths: 30 to 129 bytes

            loop {
                let res = unsafe { vradm_write_ip_packet(eng, pkt.as_ptr(), len as u32) };
                if res == VRADM_OK {
                    enqueued_clone.fetch_add(1, Ordering::Release);
                    break;
                } else if res == VRADM_ERR_QUEUE_FULL {
                    // Backoff and retry
                    thread::yield_now();
                } else {
                    panic!("Unexpected error from write_ip_packet: {}", res);
                }
            }
        }
        done_host.store(true, Ordering::Release);
    });

    // 2. Telemetry Thread: reads telemetry snapshots concurrently
    let telem_thread = thread::spawn(move || {
        let eng = engine_addr as *const vradm_engine_t;
        let mut last_frames_tx = 0u32;
        let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        let mut reads = 0u64;

        while !done_telem.load(Ordering::Acquire) || reads < 1000 {
            unsafe { vradm_get_telemetry(eng, &mut telem) };
            assert!(
                telem.frames_transmitted >= last_frames_tx,
                "Telemetry frames_transmitted went backwards: {} < {}",
                telem.frames_transmitted,
                last_frames_tx
            );
            last_frames_tx = telem.frames_transmitted;
            let mcs = unsafe { vradm_get_active_mcs(eng) };
            assert_eq!(mcs, VRADM_MCS_2);
            reads += 1;
            if reads % 50 == 0 {
                thread::yield_now();
            }
        }
        reads
    });

    // 3. Audio Render Thread: main thread drains audio
    let mut pcm_buf = [0i16; 160];
    let mut total_samples_generated = 0u64;

    // Loop until host thread is done and engine has processed packets
    while !done.load(Ordering::Acquire) || unsafe { (*engine).tx_packet_queue.len() > 0 } {
        let generated = unsafe { vradm_generate_audio(engine, pcm_buf.as_mut_ptr(), 160) };
        assert_eq!(generated, 160);
        total_samples_generated += generated as u64;
    }

    // Drain additional chunks to complete any in-flight bursts
    for _ in 0..100 {
        let generated = unsafe { vradm_generate_audio(engine, pcm_buf.as_mut_ptr(), 160) };
        total_samples_generated += generated as u64;
    }

    host_thread.join().expect("Host thread panicked");
    let telem_reads = telem_thread.join().expect("Telemetry thread panicked");

    assert_eq!(packets_enqueued.load(Ordering::Acquire), total_packets);
    assert!(total_samples_generated > 0);
    assert!(telem_reads >= 1000);

    let mut final_telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut final_telem) };
    assert!(final_telem.frames_transmitted > 0, "Expected transmitted frames");

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_concurrent_telemetry_multi_readers() {
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

    let engine_addr = engine as usize;
    let done = Arc::new(AtomicBool::new(false));
    let ready_count = Arc::new(AtomicU32::new(0));
    let num_readers = 8;
    let mut reader_handles = Vec::new();

    for reader_id in 0..num_readers {
        let done_flag = Arc::clone(&done);
        let ready_flag = Arc::clone(&ready_count);
        let handle = thread::spawn(move || {
            let eng = engine_addr as *const vradm_engine_t;
            let mut t: vradm_telemetry_t = unsafe { std::mem::zeroed() };
            ready_flag.fetch_add(1, Ordering::Release);
            let mut reads = 0u64;
            while !done_flag.load(Ordering::Acquire) || reads < 500 {
                unsafe { vradm_get_telemetry(eng, &mut t) };
                let mcs = unsafe { vradm_get_active_mcs(eng) };
                assert!(mcs <= VRADM_MCS_4);
                assert_eq!(t.frames_received, t.frames_transmitted * 2);
                assert_eq!(t.rs_corrected_bytes, t.frames_transmitted % 50);
                reads += 1;
                std::hint::spin_loop();
            }
            (reader_id, reads)
        });
        reader_handles.push(handle);
    }

    // Wait until all readers have spawned and signaled ready
    while ready_count.load(Ordering::Acquire) < num_readers {
        thread::yield_now();
    }

    // Writer: Audio thread submitting updates and generating audio
    let iterations = 20_000;
    for i in 1..=iterations {
        unsafe {
            (*engine).telem_seqlock.update(|t| {
                t.frames_transmitted = i;
                t.frames_received = i * 2;
                t.rs_corrected_bytes = i % 50;
            });
        }
        if i % 1000 == 0 {
            thread::yield_now();
        }
    }

    done.store(true, Ordering::Release);

    for handle in reader_handles {
        let (id, count) = handle.join().expect("Reader thread panicked");
        assert!(count >= 500, "Reader {} performed only {} reads", id, count);
    }

    let mut final_telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
    unsafe { vradm_get_telemetry(engine, &mut final_telem) };
    assert_eq!(final_telem.frames_transmitted, iterations);
    assert_eq!(final_telem.frames_received, iterations * 2);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_defect_arq_receiver_duplicate_frame_delivery() {
    // Demonstration of Finding 1:
    // When a retransmitted reliable frame arrives at ArqReceiver, ArqReceiver fails
    // to discard the duplicate and delivers the reassembled packet to L3 a second time.
    let mut rx = ArqReceiver::new();
    let mut frame = CanonicalDataFrame {
        ctrl: 0x02, // v3.8 IP frame
        seq: 0,
        ack_base: 0,
        ack_map: 0,
        payload_len: 10,
        payload: [0; 38],
    };
    let h = FragmentHeader {
        urgent_flush: false,
        frag_idx: 0,
        total_frags_minus_one: 0,
        best_effort: false,
    };
    frame.payload[0] = h.encode();
    frame.payload[1..10].copy_from_slice(b"test12345");

    // 1st arrival: packet is assembled and delivered
    let first_delivery = rx.receive_frame(&frame);
    assert!(first_delivery.is_some(), "First delivery of packet must succeed");

    // 2nd arrival (retransmission): receiver should discard duplicate, returning None
    let second_delivery = rx.receive_frame(&frame);
    let duplicate_delivered = second_delivery.is_some();
    
    // In buggy implementation without deduplication, duplicate_delivered is TRUE
    assert!(
        !duplicate_delivered,
        "DEFECT DETECTED: ArqReceiver re-delivered duplicate reliable frame! Expected None but got {:?}",
        second_delivery
    );
}
