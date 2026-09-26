use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration};

use vradm_core::arq::{ArqReceiver, ArqTransmitter, IpPacketSlicer, MAX_IP_PACKET_LEN, MAX_IP_FRAGMENTS};
use vradm_core::c_abi::*;
use vradm_core::framing::{seq_advance, CanonicalDataFrame};

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

fn test_config(mcs: u8) -> vradm_config_t {
    vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: mcs,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.3535,
        reserved2: [0; 4],
        psk_key: [0x5A; 16],
    }
}

// ============================================================================
// CHALLENGE 1: Extreme Bursty Backlog Zero-Allocation Verification
// ============================================================================

#[test]
fn test_challenge_bursty_queue_saturation_zero_alloc() {
    let cfg = test_config(VRADM_MCS_2);
    let engine_tx = unsafe { vradm_create(&cfg) };
    let engine_rx = unsafe { vradm_create(&cfg) };
    assert!(!engine_tx.is_null());
    assert!(!engine_rx.is_null());

    // 1. Flood tx_packet_queue to capacity with maximum-size packets (296 bytes = 8 frags each)
    let max_pkt = [0xDEu8; MAX_IP_PACKET_LEN];
    let mut initial_queued = 0;
    loop {
        let res = unsafe { vradm_write_ip_packet(engine_tx, max_pkt.as_ptr(), max_pkt.len() as u32) };
        if res == VRADM_OK {
            initial_queued += 1;
        } else {
            assert_eq!(res, VRADM_ERR_QUEUE_FULL);
            break;
        }
    }
    // SPSC queue capacity is 64 (holds 63 or 64 slots)
    assert!(initial_queued >= 63, "Queue should accept at least 63 packets, got {}", initial_queued);

    // 2. Begin allocation tracking strictly on the audio processing thread
    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));

    let mut chunk = [0i16; 160];
    
    let mut packets_refilled = 0;

    // Run 5,000 audio chunks (100 seconds of simulated real-time 8 kHz audio)
    for i in 0..5_000 {
        let n = unsafe { vradm_generate_audio(engine_tx, chunk.as_mut_ptr(), 160) };
        assert_eq!(n, 160);

        unsafe { vradm_process_audio(engine_rx, chunk.as_ptr(), 160) };
        

        // Whenever audio generation frees up space in tx_packet_queue, replenish it
        // We temporarily pause tracking when generating payload from test runner if needed,
        // but here write_ip_packet runs on this thread as well. Let's see if write_ip_packet allocates!
        // In fact, write_ip_packet operates on fixed-size SpscQueue<PacketSlot, 64>, which is zero-alloc!
        if i % 10 == 0 {
            let res = unsafe { vradm_write_ip_packet(engine_tx, max_pkt.as_ptr(), max_pkt.len() as u32) };
            if res == VRADM_OK {
                packets_refilled += 1;
            }
        }
    }

    THREAD_TRACKING.with(|t| t.set(false));
    let allocs = THREAD_ALLOC_COUNT.with(|c| c.get());
    let deallocs = THREAD_DEALLOC_COUNT.with(|c| c.get());

    println!(
        "[EMPIRICAL CHALLENGE 1] Bursty queue saturation (5000 chunks, {} refilled pkts): {} allocs, {} deallocs",
        packets_refilled, allocs, deallocs
    );

    unsafe {
        vradm_destroy(engine_tx);
        vradm_destroy(engine_rx);
    }

    assert_eq!(
        allocs, 0,
        "ZERO-ALLOCATION VIOLATION: {} heap allocations detected during extreme burst audio processing",
        allocs
    );
    assert_eq!(
        deallocs, 0,
        "ZERO-ALLOCATION VIOLATION: {} heap deallocations detected during extreme burst audio processing",
        deallocs
    );
}

#[test]
fn test_challenge_arq_queue_capacity_overflow_safety_and_zero_alloc() {
    let mut tx = ArqTransmitter::new();
    let packet = [0x55u8; MAX_IP_PACKET_LEN]; // 8 fragments per packet

    THREAD_ALLOC_COUNT.with(|c| c.set(0));
    THREAD_DEALLOC_COUNT.with(|c| c.set(0));
    THREAD_TRACKING.with(|t| t.set(true));

    // Fill ArqTransmitter pending_reliable to its maximum capacity (1024 frames = 128 packets)
    let mut packets_accepted = 0;
    for _ in 0..150 {
        if tx.enqueue_packet(&packet, false, false).is_ok() {
            packets_accepted += 1;
        }
    }

    // Must accept exactly 128 packets (128 * 8 = 1024 frames)
    assert_eq!(packets_accepted, 128, "Expected 128 packets (1024 frames) to fill pending_reliable");
    assert_eq!(tx.pending_reliable.len(), 1024);

    // Any further packet enqueue MUST be rejected with error and MUST NOT reallocate or grow
    let overflow_res = tx.enqueue_packet(&packet, false, false);
    assert!(overflow_res.is_err(), "Overflow packet must be rejected");
    assert_eq!(tx.pending_reliable.len(), 1024);
    assert_eq!(tx.pending_reliable.capacity(), 1024);

    THREAD_TRACKING.with(|t| t.set(false));
    let allocs = THREAD_ALLOC_COUNT.with(|c| c.get());
    let deallocs = THREAD_DEALLOC_COUNT.with(|c| c.get());

    println!(
        "[EMPIRICAL CHALLENGE 1b] ARQ queue capacity saturation: 1024 frames enqueued, allocs={}, deallocs={}",
        allocs, deallocs
    );

    assert_eq!(allocs, 0, "No allocations allowed when saturating ARQ capacity");
    assert_eq!(deallocs, 0, "No deallocations allowed when saturating ARQ capacity");
}

// ============================================================================
// CHALLENGE 2: Modulo-256 Sequence Wraparound Continuous Delivery (Past 1024 pkts)
// ============================================================================

#[test]
fn test_challenge_modulo_256_wraparound_continuous_stream_1024_packets() {
    let mut rx = ArqReceiver::new();
    let mut packet_buf = [0u8; MAX_IP_PACKET_LEN];

    let total_packets_to_test = 1024 + 64; // 1088 packets (> 4 full 256-sequence cycles)
    let mut delivered_count = 0;
    let mut current_seq = 0u8;

    for pkt_idx in 0..total_packets_to_test {
        // Vary packet sizes: 1, 2, 4, 8 fragments
        let frags = match pkt_idx % 4 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        };
        let payload_per_frag = 37usize;
        let total_payload_bytes = frags * payload_per_frag;
        let mut test_payload = vec![0u8; total_payload_bytes];
        for (b, byte) in test_payload.iter_mut().enumerate() {
            *byte = ((pkt_idx * 7 + b) & 0xFF) as u8;
        }

        // Slicer frames
        let mut frames = [CanonicalDataFrame::new(); MAX_IP_FRAGMENTS];
        let n_frags = IpPacketSlicer::slice_into(
            &test_payload,
            false,
            false,
            current_seq,
            &mut frames,
        ).expect("Slicing failed");
        assert_eq!(n_frags, frags);

        current_seq = seq_advance(current_seq, frags as u8);

        // Ingest all frames for this packet
        let mut delivered_packet = None;
        for frame in frames.iter().take(n_frags) {
            if let Some(len) = rx.receive_frame_into(frame, &mut packet_buf) {
                delivered_packet = Some(packet_buf[..len].to_vec());
            }
        }

        assert!(
            delivered_packet.is_some(),
            "Packet {} (seq before wrap: {}, frags: {}) failed delivery!",
            pkt_idx, current_seq, frags
        );

        let delivered_bytes = delivered_packet.unwrap();
        assert_eq!(
            delivered_bytes, test_payload,
            "Packet {} data corruption across sequence wraparound!",
            pkt_idx
        );
        delivered_count += 1;
    }

    println!(
        "[EMPIRICAL CHALLENGE 2a] Continuous delivery across {} packets ({} full cycles): delivered = {}",
        total_packets_to_test,
        total_packets_to_test / 256,
        delivered_count
    );

    assert_eq!(
        delivered_count, total_packets_to_test,
        "Every packet across 1088 sequence cycles must be delivered without drop!"
    );
}

#[test]
fn test_challenge_modulo_256_wraparound_loss_recovery_and_sack() {
    // Tests packet loss and SACK selective retransmissions directly spanning the 255 -> 0 wraparound
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::with_initial_seq(252);
    let mut packet_buf = [0u8; MAX_IP_PACKET_LEN];

    // Align transmitter next_seq to 252
    tx.next_seq = 252;

    // Send packet A: 296 bytes (8 frags), spanning seq 252..3 (252, 253, 254, 255, 0, 1, 2, 3)
    let pkt_a = [0x41u8; MAX_IP_PACKET_LEN];
    tx.enqueue_packet(&pkt_a, false, false).unwrap();

    let mut frames_a = [CanonicalDataFrame::new(); 8];
    let n = tx.get_frames_to_transmit_into(8, &mut frames_a);
    assert_eq!(n, 8);

    // Simulate loss: Frame 0 (seq 252) and Frame 4 (seq 0) are dropped in transmission!
    // Frames received: seq 253, 254, 255, 1, 2, 3
    let del_1 = rx.receive_frame_into(&frames_a[1], &mut packet_buf); // seq 253
    assert!(del_1.is_none());
    let del_2 = rx.receive_frame_into(&frames_a[2], &mut packet_buf); // seq 254
    assert!(del_2.is_none());
    let del_3 = rx.receive_frame_into(&frames_a[3], &mut packet_buf); // seq 255
    assert!(del_3.is_none());
    // frames_a[4] (seq 0) dropped!
    let del_5 = rx.receive_frame_into(&frames_a[5], &mut packet_buf); // seq 1
    assert!(del_5.is_none());
    let del_6 = rx.receive_frame_into(&frames_a[6], &mut packet_buf); // seq 2
    assert!(del_6.is_none());
    let del_7 = rx.receive_frame_into(&frames_a[7], &mut packet_buf); // seq 3
    assert!(del_7.is_none());

    // Feed SACK feedback back to transmitter
    tx.on_ack_received(rx.ack_base, rx.ack_map);

    // Retransmit remaining unacked frames
    let mut retrans_frames = [CanonicalDataFrame::new(); 8];
    let n_retrans = tx.get_frames_to_transmit_into(8, &mut retrans_frames);
    // ACK_BASE=251 covers only 252..2 in the seven selective bits.
    // Seq 3 is received but cannot be reported until the cumulative base moves.
    assert_eq!(n_retrans, 3, "Missing frames plus the unrepresentable eighth frame");
    assert_eq!(retrans_frames[2].seq, 3);
    assert_eq!(retrans_frames[0].seq, 252);
    assert_eq!(retrans_frames[1].seq, 0);

    // Deliver seq 252
    let del_retrans_0 = rx.receive_frame_into(&retrans_frames[0], &mut packet_buf);
    assert!(del_retrans_0.is_none(), "Packet not complete yet (seq 0 still missing)");

    // Deliver seq 0 -> Packet must now complete!
    let del_retrans_4 = rx.receive_frame_into(&retrans_frames[1], &mut packet_buf);
    assert!(del_retrans_4.is_some(), "Packet A must be completed and delivered on final retransmission");
    let delivered_len = del_retrans_4.unwrap();
    assert_eq!(delivered_len, MAX_IP_PACKET_LEN);
    assert_eq!(&packet_buf[..delivered_len], &pkt_a[..]);

    // Send ACK back to transmitter
    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert_eq!(tx.in_flight.len(), 0, "All frames should be cleared from in_flight after delivery");

    println!(
        "[EMPIRICAL CHALLENGE 2b] Modulo-256 wraparound loss recovery succeeded: rx.ack_base={}, in_flight={}",
        rx.ack_base, tx.in_flight.len()
    );
}

// ============================================================================
// CHALLENGE 3: Multithreaded Concurrency Torture Across C-ABI Entry Points
// ============================================================================

#[test]
fn test_challenge_multithreaded_concurrency_torture() {
    let cfg = test_config(VRADM_MCS_2);
    let engine = unsafe { vradm_create(&cfg) };
    assert!(!engine.is_null());

    let engine_addr = engine as usize;
    let running = Arc::new(AtomicBool::new(true));

    let packets_sent = Arc::new(AtomicU64::new(0));
    let packets_polled = Arc::new(AtomicU64::new(0));
    let telemetry_reads = Arc::new(AtomicU64::new(0));
    let commands_sent = Arc::new(AtomicU64::new(0));
    let audio_chunks_rendered = Arc::new(AtomicU64::new(0));

    // Thread 1: Host Network TX Thread (Continuous packet enqueue with varied types)
    let r_tx = Arc::clone(&running);
    let cnt_tx = Arc::clone(&packets_sent);
    let tx_handle = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut pkt = [0u8; MAX_IP_PACKET_LEN];
        let mut seq = 0u32;
        while r_tx.load(Ordering::Acquire) {
            seq = seq.wrapping_add(1);
            let len = 28 + (seq as usize % (MAX_IP_PACKET_LEN - 28));
            // IPv4 header
            pkt[0] = 0x45;
            pkt[1] = (seq >> 8) as u8;
            pkt[2] = (seq & 0xFF) as u8;

            if seq % 3 == 0 {
                // UDP port 60001 (best effort)
                pkt[9] = 17;
                pkt[22] = 0xEA;
                pkt[23] = 0x61;
            } else if seq % 3 == 1 {
                // TCP with URG flag
                pkt[9] = 6;
                let ihl = 20;
                pkt[ihl + 13] = 0x20;
            } else {
                // Normal TCP
                pkt[9] = 6;
                pkt[33] = 0x00;
            }

            let res = unsafe { vradm_write_ip_packet(eng, pkt.as_ptr(), len as u32) };
            if res == VRADM_OK {
                cnt_tx.fetch_add(1, Ordering::Relaxed);
            } else {
                thread::yield_now();
            }
        }
    });

    // Thread 2: Host Network RX Thread (Continuous packet polling)
    let r_rx = Arc::clone(&running);
    let cnt_rx = Arc::clone(&packets_polled);
    let rx_handle = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut out = [0u8; MAX_IP_PACKET_LEN];
        while r_rx.load(Ordering::Acquire) {
            let res = unsafe { vradm_poll_ip_packet(eng, out.as_mut_ptr(), MAX_IP_PACKET_LEN as u32) };
            if res > 0 {
                cnt_rx.fetch_add(1, Ordering::Relaxed);
                assert_eq!(out[0], 0x45, "Packet corrupted during concurrent polling");
            } else {
                thread::yield_now();
            }
        }
    });

    // Thread 3: Telemetry Reader Thread
    let r_telem = Arc::clone(&running);
    let cnt_telem = Arc::clone(&telemetry_reads);
    let telem_handle = thread::spawn(move || {
        let eng = engine_addr as *const vradm_engine_t;
        let mut telem: vradm_telemetry_t = unsafe { std::mem::zeroed() };
        let mut prev_tx = 0u32;
        while r_telem.load(Ordering::Acquire) {
            unsafe { vradm_get_telemetry(eng, &mut telem) };
            // Ensure monotonic frames_transmitted
            assert!(
                telem.frames_transmitted >= prev_tx,
                "Non-monotonic telemetry read: {} < {}",
                telem.frames_transmitted,
                prev_tx
            );
            prev_tx = telem.frames_transmitted;

            let mcs = unsafe { vradm_get_active_mcs(eng) };
            assert!(mcs >= VRADM_MCS_2 && mcs <= VRADM_MCS_4, "Invalid MCS read: {}", mcs);
            cnt_telem.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Thread 4: Command Submitter Thread (Rapid MCS switching and commands)
    let r_cmd = Arc::clone(&running);
    let cnt_cmd = Arc::clone(&commands_sent);
    let cmd_handle = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut cid = 0u32;
        while r_cmd.load(Ordering::Acquire) {
            cid += 1;
            let target_mcs = 2 + (cid % 3); // Cycles 2, 3, 4
            let cmd = vradm_cmd_t {
                cmd_type: VRADM_CMD_REQUEST_MCS,
                cmd_id: cid,
                param_u32: target_mcs,
                param_i32: 0,
                param_f32: 0.0,
                inline_payload: [0; 12],
            };
            let res = unsafe { vradm_submit_cmd(eng, &cmd) };
            if res == VRADM_OK {
                cnt_cmd.fetch_add(1, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(1));
        }
    });

    // Thread 5: Real-Time Audio Render & Ingest Thread (Simulated Audio Engine)
    let r_audio = Arc::clone(&running);
    let cnt_audio = Arc::clone(&audio_chunks_rendered);
    let audio_handle = thread::spawn(move || {
        let eng = engine_addr as *mut vradm_engine_t;
        let mut out_pcm = [0i16; 160];
        while r_audio.load(Ordering::Acquire) {
            let n = unsafe { vradm_generate_audio(eng, out_pcm.as_mut_ptr(), 160) };
            assert_eq!(n, 160);
            unsafe { vradm_process_audio(eng, out_pcm.as_ptr(), 160) };
            cnt_audio.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Run torture test for 1,500 ms
    thread::sleep(Duration::from_millis(1500));
    running.store(false, Ordering::Release);

    tx_handle.join().expect("TX thread panicked");
    rx_handle.join().expect("RX thread panicked");
    telem_handle.join().expect("Telemetry thread panicked");
    cmd_handle.join().expect("Command thread panicked");
    audio_handle.join().expect("Audio thread panicked");

    let n_tx = packets_sent.load(Ordering::Acquire);
    let n_rx = packets_polled.load(Ordering::Acquire);
    let n_telem = telemetry_reads.load(Ordering::Acquire);
    let n_cmd = commands_sent.load(Ordering::Acquire);
    let n_audio = audio_chunks_rendered.load(Ordering::Acquire);

    println!(
        "[EMPIRICAL CHALLENGE 3] Multithreaded C-ABI torture completed successfully:\n  TX Packets: {}\n  RX Packets: {}\n  Telemetry Reads: {}\n  Commands Sent: {}\n  Audio Chunks: {}",
        n_tx, n_rx, n_telem, n_cmd, n_audio
    );

    assert!(n_tx > 100, "High TX throughput required");
    assert!(n_telem > 5_000, "High telemetry throughput required");
    assert!(n_audio > 100, "Audio engine must have sustained continuous generation");

    unsafe { vradm_destroy(engine) };
}

// ============================================================================
// CHALLENGE 4: End-to-End Closed Loop Across Modulo-256 Wraparound
// ============================================================================

#[test]
fn test_challenge_end_to_end_closed_loop_wraparound() {
    let cfg = test_config(VRADM_MCS_2);
    let engine_tx = unsafe { vradm_create(&cfg) };
    let engine_rx = unsafe { vradm_create(&cfg) };
    assert!(!engine_tx.is_null());
    assert!(!engine_rx.is_null());

    let mut pcm_buf = [0i16; 160];
    let mut rx_packet_buf = [0u8; MAX_IP_PACKET_LEN];

    // Deliver 300 packets end-to-end through the full audio pipeline
    // This traverses the modulo-256 sequence boundary (0..255..299)
    let total_packets = 300;
    let mut delivered = 0;

    for i in 0..total_packets {
        let len = 40 + (i % 50);
        let mut pkt = vec![0u8; len];
        pkt[0] = 0x45;
        pkt[1] = (i >> 8) as u8;
        pkt[2] = (i & 0xFF) as u8;
        for j in 3..len {
            pkt[j] = ((i * 3 + j) & 0xFF) as u8;
        }

        let write_res = unsafe { vradm_write_ip_packet(engine_tx, pkt.as_ptr(), len as u32) };
        assert_eq!(write_res, VRADM_OK, "Packet {} write failed", i);

        // Run audio loopback until packet is decoded at receiver
        let mut cycles = 0;
        let mut packet_received = false;

        while cycles < 600 {
            cycles += 1;
            // 1. TX generates audio
            let n_gen = unsafe { vradm_generate_audio(engine_tx, pcm_buf.as_mut_ptr(), 160) };
            assert_eq!(n_gen, 160);

            // 2. RX processes audio
            unsafe { vradm_process_audio(engine_rx, pcm_buf.as_ptr(), 160) };

            // 3. Check if packet is decoded at RX
            let poll_len = unsafe { vradm_poll_ip_packet(engine_rx, rx_packet_buf.as_mut_ptr(), MAX_IP_PACKET_LEN as u32) };
            if poll_len > 0 {
                assert_eq!(poll_len as usize, len, "Packet {} length mismatch", i);
                assert_eq!(&rx_packet_buf[..poll_len as usize], &pkt[..], "Packet {} data corrupted", i);
                packet_received = true;
                delivered += 1;
                break;
            }

            // Also loop back RX audio to TX to return ACKs / TDD turn!
            let mut ack_pcm = [0i16; 160];
            let n_ack = unsafe { vradm_generate_audio(engine_rx, ack_pcm.as_mut_ptr(), 160) };
            if n_ack > 0 {
                unsafe { vradm_process_audio(engine_tx, ack_pcm.as_ptr(), n_ack) };
            }
        }

        assert!(
            packet_received,
            "Packet {} was not received within 600 audio cycles (wraparound stall!)",
            i
        );
    }

    println!(
        "[EMPIRICAL CHALLENGE 4] Full closed-loop audio pipeline successfully delivered {} / {} packets across sequence wraparound",
        delivered, total_packets
    );

    assert_eq!(delivered, total_packets);

    unsafe {
        vradm_destroy(engine_tx);
        vradm_destroy(engine_rx);
    }
}
