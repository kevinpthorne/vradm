use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use vradm_core::arq::{ArqReceiver, FragmentHeader, MAX_IP_PACKET_LEN};
use vradm_core::c_abi::*;
use vradm_core::framing::CanonicalDataFrame;

// ============================================================================
// Tracking Allocator for Empirical Zero-Allocation Verification
// ============================================================================

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

fn test_config() -> vradm_config_t {
    vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x3C; 16],
    }
}

// ============================================================================
// CHALLENGE 1: Empirical Zero-Allocation Verification & Defect Identification
// ============================================================================

#[test]
fn test_defect_zero_alloc_pending_reliable_growth_on_audio_thread() {
    // EMPIRICAL CHALLENGE 1:
    // ArqTransmitter initializes pending_reliable with capacity 32:
    //   pending_reliable: VecDeque::with_capacity(32)
    // When the host queues multiple large IP packets (e.g. 6 x 296B = 48 frames),
    // and audio is generated without immediate ACKs, pending_reliable accumulates
    // frames beyond capacity 32, causing VecDeque to reallocate on the heap
    // DIRECTLY INSIDE vradm_generate_audio ON THE REAL-TIME AUDIO THREAD.
    let cfg = test_config();
    let engine_tx = unsafe { vradm_create(&cfg) };
    assert!(!engine_tx.is_null());

    // Host enqueues 6 packets of 296 bytes (8 frags each = 48 frames total)
    let packet = [0xA5u8; 296];
    for i in 0..6 {
        let res = unsafe { vradm_write_ip_packet(engine_tx, packet.as_ptr(), packet.len() as u32) };
        assert_eq!(res, VRADM_OK, "Packet {} enqueue failed", i);
    }

    // Begin tracking allocations strictly on this audio thread
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));

    let mut chunk = [0i16; 160];
    let mut chunks_run = 0;

    // Run audio generation across 1500 chunks (~5 complete bursts)
    for _ in 0..1500 {
        let n = unsafe { vradm_generate_audio(engine_tx, chunk.as_mut_ptr(), 160) };
        assert_eq!(n, 160);
        chunks_run += 1;
    }

    THREAD_TRACKING.with(|t| t.set(false));
    let allocs = THREAD_ALLOC_COUNT.with(|c| c.get());
    let deallocs = THREAD_DEALLOC_COUNT.with(|c| c.get());

    println!(
        "[CHALLENGE 1 RESULT] Bursty queue audio generation across {} chunks: {} allocations, {} deallocations",
        chunks_run, allocs, deallocs
    );

    unsafe { vradm_destroy(engine_tx) };

    assert_eq!(
        allocs, 0,
        "ZERO-ALLOCATION INVARIANT VIOLATION: {} heap allocations on audio thread",
        allocs
    );
    assert_eq!(
        deallocs, 0,
        "ZERO-ALLOCATION INVARIANT VIOLATION: {} heap deallocations on audio thread",
        deallocs
    );
}

#[test]
fn test_zero_alloc_nominal_single_packet_and_noise() {
    // Demonstrates that when traffic does not exceed capacity 32, the audio pipeline
    // and noise ingest paths remain zero-allocation.
    let cfg = test_config();
    let engine_tx = unsafe { vradm_create(&cfg) };
    let engine_rx = unsafe { vradm_create(&cfg) };

    let packet = [0x5Au8; 100]; // 3 fragments
    unsafe { vradm_write_ip_packet(engine_tx, packet.as_ptr(), 100) };

    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));

    let mut chunk = [0i16; 160];
    for _ in 0..100 {
        unsafe {
            vradm_generate_audio(engine_tx, chunk.as_mut_ptr(), 160);
            vradm_process_audio(engine_rx, chunk.as_ptr(), 160);
        }
    }

    THREAD_TRACKING.with(|t| t.set(false));
    let allocs = THREAD_ALLOC_COUNT.with(|c| c.get());
    let deallocs = THREAD_DEALLOC_COUNT.with(|c| c.get());

    unsafe {
        vradm_destroy(engine_tx);
        vradm_destroy(engine_rx);
    }

    assert_eq!(allocs, 0, "Nominal traffic must be 0 allocations");
    assert_eq!(deallocs, 0, "Nominal traffic must be 0 deallocations");
}

// ============================================================================
// CHALLENGE 2: Duplicate Frame Injection & Context Starvation Defect
// ============================================================================

#[test]
fn test_duplicate_frame_rejection_in_window() {
    // Verify that ArqReceiver successfully rejects duplicate frames in-window
    // for both single-fragment and 8-fragment packets when flooded with 10,000 duplicates.
    let mut rx = ArqReceiver::new();
    let mut packet_buf = [0u8; MAX_IP_PACKET_LEN];

    // Single-fragment
    let mut frame0 = CanonicalDataFrame::new();
    frame0.ctrl = 0x02; // Current v3.8 IP wire version
    frame0.seq = 0;
    let payload_data = b"single_frag_test";
    frame0.payload_len = (payload_data.len() + 1) as u8;
    let h0 = FragmentHeader {
        urgent_flush: false,
        frag_idx: 0,
        total_frags_minus_one: 0,
        best_effort: false,
    };
    frame0.payload[0] = h0.encode();
    frame0.payload[1..1 + payload_data.len()].copy_from_slice(payload_data);

    let first = rx.receive_frame_into(&frame0, &mut packet_buf);
    assert!(first.is_some(), "First delivery of frame0 must succeed");

    let mut dup_count = 0;
    for _ in 0..10_000 {
        if rx.receive_frame_into(&frame0, &mut packet_buf).is_some() {
            dup_count += 1;
        }
    }
    assert_eq!(dup_count, 0, "Zero duplicates allowed for single fragment");

    // Multi-fragment (8 fragments)
    rx.reset();
    let total_frags = 8;
    let mut frames = [CanonicalDataFrame::new(); 8];
    for f in 0..total_frags {
        frames[f].ctrl = 0x02; // Current v3.8 IP wire version
        frames[f].seq = f as u8;
        frames[f].payload_len = 21;
        let h = FragmentHeader {
            urgent_flush: false,
            frag_idx: f as u8,
            total_frags_minus_one: (total_frags - 1) as u8,
            best_effort: false,
        };
        frames[f].payload[0] = h.encode();
        frames[f].payload[1..21].fill(0x30 + f as u8);
    }

    for f in 0..7 {
        assert!(rx.receive_frame_into(&frames[f], &mut packet_buf).is_none());
        assert!(rx.receive_frame_into(&frames[f], &mut packet_buf).is_none());
    }
    let del = rx.receive_frame_into(&frames[7], &mut packet_buf);
    assert!(del.is_some(), "8-fragment packet must be delivered");

    let mut multi_dup_count = 0;
    for i in 0..10_000 {
        if rx.receive_frame_into(&frames[i % 8], &mut packet_buf).is_some() {
            multi_dup_count += 1;
        }
    }
    assert_eq!(multi_dup_count, 0, "Zero duplicates allowed for multi-fragment");
}

#[test]
fn test_defect_arq_context_eviction_starvation_and_wraparound_packet_loss() {
    // EMPIRICAL CHALLENGE 2:
    // In ArqReceiver::receive_frame_into (src/arq.rs:387), context eviction does:
    //   for (i, slot) in self.contexts.iter().enumerate() {
    //       if let Some(ctx) = slot {
    //           if ctx.delivered {
    //               evict_idx = i;
    //               break;
    //           }
    //       }
    //   }
    // Because slot 0 is always delivered, it ALWAYS selects evict_idx = 0.
    // Slots 1..15 are NEVER evicted!
    // When sequence numbers wrap around modulo 256, packet 257 (pkt_id = 1)
    // matches slot 1, where ctx.delivered is still true from 256 packets ago!
    // Deduplication Check 3 (line 406) unconditionally returns None!
    // This causes packets 257 through 271 (15 consecutive packets) to be permanently dropped!
    let mut rx = ArqReceiver::new();
    let mut packet_buf = [0u8; MAX_IP_PACKET_LEN];

    // 1. Deliver packets 0..255 (full 256 sequence cycle)
    for seq in 0..256 {
        let mut frame = CanonicalDataFrame::new();
        frame.seq = seq as u8;
        frame.payload_len = 5;
        let h = FragmentHeader {
            urgent_flush: false,
            frag_idx: 0,
            total_frags_minus_one: 0,
            best_effort: false,
        };
        frame.payload[0] = h.encode();
        frame.payload[1..5].fill(seq as u8);
        let del = rx.receive_frame_into(&frame, &mut packet_buf);
        assert!(del.is_some(), "Initial delivery of packet {} must succeed", seq);
    }

    // 2. Deliver packet 256 (seq 0 in modulo-256): evicts slot 0 and succeeds
    let mut f256 = CanonicalDataFrame::new();
    f256.seq = 0;
    f256.payload_len = 5;
    let h = FragmentHeader {
        urgent_flush: false,
        frag_idx: 0,
        total_frags_minus_one: 0,
        best_effort: false,
    };
    f256.payload[0] = h.encode();
    f256.payload[1..5].fill(0xAA);
    let del256 = rx.receive_frame_into(&f256, &mut packet_buf);
    assert!(del256.is_some(), "Packet 256 (seq 0) must be delivered");

    // 3. Attempt to deliver packet 257 (seq 1 in modulo-256):
    let mut f257 = CanonicalDataFrame::new();
    f257.seq = 1;
    f257.payload_len = 5;
    f257.payload[0] = h.encode();
    f257.payload[1..5].fill(0xBB);
    let del257 = rx.receive_frame_into(&f257, &mut packet_buf);

    println!(
        "[CHALLENGE 2 RESULT] Packet 257 delivery after sequence wrap: del257 = {:?}",
        del257
    );

    assert!(
        del257.is_some(),
        "Packet 257 should be successfully delivered across modulo-256 wraparound!"
    );
}

// ============================================================================
// CHALLENGE 3: Concurrent C-ABI Stress & MCS 4 Panic Defect
// ============================================================================

#[test]
fn test_defect_mcs4_modulation_buffer_overflow_panic() {
    // EMPIRICAL CHALLENGE 3a:
    // In src/phy.rs:406, DqpskModulator::reset_for_mcs matches `_ =>` for MCS 4,
    // setting num_carriers = 4 instead of 8.
    // In modulate_frame_into (src/phy.rs:422), total_symbols is set to 33 (1320 samples),
    // but the modulation loop with 4 carriers runs for 65 symbols (2600 samples).
    // When VRADM_CMD_REQUEST_MCS selects MCS 4, modulate_burst panics at sample 1320:
    //   index out of bounds: the len is 1320 but the index is 1320
    let cfg = test_config();
    let engine = unsafe { vradm_create(&cfg) };
    assert!(!engine.is_null());

    // Submit request for MCS 4
    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 1,
        param_u32: 4, // MCS 4
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };
    unsafe { vradm_submit_cmd(engine, &cmd) };

    // Enqueue an IP packet so an audio burst is synthesized in MCS 4
    let pkt = [0x55u8; 100];
    unsafe { vradm_write_ip_packet(engine, pkt.as_ptr(), 100) };

    // Catch panic when generate_audio attempts to synthesize MCS 4 burst
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pcm = [0i16; 160];
        unsafe { vradm_generate_audio(engine, pcm.as_mut_ptr(), 160) };
    }));

    println!(
        "[CHALLENGE 3a RESULT] generate_audio in MCS 4: Panicked = {}",
        result.is_err()
    );

    unsafe { vradm_destroy(engine) };

    assert!(
        result.is_ok(),
        "MCS 4 modulation panicked during generate_audio!"
    );
}

#[test]
fn test_concurrent_c_abi_audio_packet_telemetry_mcs2_mcs3() {
    // EMPIRICAL CHALLENGE 3b:
    // Stress test concurrent C-ABI access across:
    //   - Audio Render Thread (generate_audio + process_audio)
    //   - Host Network TX Thread (write_ip_packet)
    //   - Host Network RX Thread (poll_ip_packet)
    //   - Telemetry Monitor Thread (get_telemetry seqlock + get_active_mcs)
    //   - Command Thread (submit_cmd switching MCS 2 <-> 3)
    let cfg = test_config();
    let engine = unsafe { vradm_create(&cfg) };
    assert!(!engine.is_null());

    let engine_addr = engine as usize;
    let running = Arc::new(AtomicBool::new(true));
    let packets_sent = Arc::new(AtomicU64::new(0));
    let packets_polled = Arc::new(AtomicU64::new(0));
    let telem_reads = Arc::new(AtomicU64::new(0));
    let cmds_sent = Arc::new(AtomicU64::new(0));

    // 1. Host Network TX Thread
    let running_tx = Arc::clone(&running);
    let sent_counter = Arc::clone(&packets_sent);
    let tx_thread = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut pkt = [0u8; 256];
        let mut seq = 0u32;
        while running_tx.load(Ordering::Acquire) {
            seq += 1;
            pkt[0] = 0x45; // IPv4
            pkt[1] = (seq >> 8) as u8;
            pkt[2] = (seq & 0xFF) as u8;
            let len = 28 + (seq % 200) as usize;

            let res = unsafe { vradm_write_ip_packet(eng, pkt.as_ptr(), len as u32) };
            if res == VRADM_OK {
                sent_counter.fetch_add(1, Ordering::Relaxed);
            } else if res == VRADM_ERR_QUEUE_FULL {
                thread::yield_now();
            }
        }
    });

    // 2. Host Network RX Thread
    let running_rx = Arc::clone(&running);
    let poll_counter = Arc::clone(&packets_polled);
    let rx_thread = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut out_buf = [0u8; MAX_IP_PACKET_LEN];
        while running_rx.load(Ordering::Acquire) {
            let res = unsafe { vradm_poll_ip_packet(eng, out_buf.as_mut_ptr(), MAX_IP_PACKET_LEN as u32) };
            if res > 0 {
                poll_counter.fetch_add(1, Ordering::Relaxed);
                assert_eq!(out_buf[0], 0x45, "Corrupted IP packet header");
            } else {
                thread::yield_now();
            }
        }
    });

    // 3. Telemetry Monitor Thread
    let running_telem = Arc::clone(&running);
    let telem_counter = Arc::clone(&telem_reads);
    let telem_thread = thread::spawn(move || {
        let eng = engine_addr as *const vradm_engine_t;
        let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        let mut last_tx = 0u32;
        while running_telem.load(Ordering::Acquire) {
            unsafe { vradm_get_telemetry(eng, &mut telem) };
            assert!(
                telem.frames_transmitted >= last_tx,
                "Telemetry counter inversion: {} < {}",
                telem.frames_transmitted,
                last_tx
            );
            last_tx = telem.frames_transmitted;
            let mcs = unsafe { vradm_get_active_mcs(eng) };
            assert!(mcs == VRADM_MCS_2 || mcs == VRADM_MCS_3, "Unexpected MCS: {}", mcs);
            telem_counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    // 4. Command Thread
    let running_cmd = Arc::clone(&running);
    let cmd_counter = Arc::clone(&cmds_sent);
    let cmd_thread = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut cmd_id = 0u32;
        while running_cmd.load(Ordering::Acquire) {
            cmd_id += 1;
            let cmd = vradm_cmd_t {
                cmd_type: VRADM_CMD_REQUEST_MCS,
                cmd_id,
                param_u32: 2 + (cmd_id % 2), // cycle MCS 2 and 3
                param_i32: 0,
                param_f32: 0.0,
                inline_payload: [0; 12],
            };
            let res = unsafe { vradm_submit_cmd(eng, &cmd) };
            if res == VRADM_OK {
                cmd_counter.fetch_add(1, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(2));
        }
    });

    // 5. Audio Render Thread (Main Thread)
    let mut pcm_out = [0i16; 160];
    let start_time = std::time::Instant::now();
    let duration = Duration::from_millis(800);

    while start_time.elapsed() < duration {
        let n = unsafe { vradm_generate_audio(engine, pcm_out.as_mut_ptr(), 160) };
        assert_eq!(n, 160);
        unsafe { vradm_process_audio(engine, pcm_out.as_ptr(), 160) };
    }

    running.store(false, Ordering::Release);

    tx_thread.join().expect("TX thread panicked");
    rx_thread.join().expect("RX thread panicked");
    telem_thread.join().expect("Telemetry thread panicked");
    cmd_thread.join().expect("Command thread panicked");

    let total_tx = packets_sent.load(Ordering::Acquire);
    let total_rx = packets_polled.load(Ordering::Acquire);
    let total_telem = telem_reads.load(Ordering::Acquire);
    let total_cmds = cmds_sent.load(Ordering::Acquire);

    println!(
        "[CHALLENGE 3b RESULT] Concurrent C-ABI stress completed successfully:\n  TX Packets Enqueued: {}\n  RX Packets Polled:   {}\n  Telemetry Reads:     {}\n  Commands Applied:    {}",
        total_tx, total_rx, total_telem, total_cmds
    );

    assert!(total_tx > 50, "Expected significant packet enqueue activity");
    assert!(total_telem > 1000, "Expected high telemetry read volume");

    unsafe { vradm_destroy(engine) };
}
