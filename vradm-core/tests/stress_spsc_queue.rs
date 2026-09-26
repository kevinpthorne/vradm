use std::thread;
use vradm_core::c_abi::*;
use vradm_core::engine::SpscQueue;

#[test]
fn test_spsc_queue_capacity_saturation_and_fifo() {
    let mut queue: SpscQueue<u32, 64> = SpscQueue::new();
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);

    // 1. Fill queue to exact capacity of 64
    for i in 0..64 {
        assert_eq!(queue.push(i), Ok(()), "Push failed at index {}", i);
        assert_eq!(queue.len(), i as usize + 1);
    }

    // 2. 65th push must fail (Queue Full)
    assert_eq!(queue.push(999), Err(999), "65th push should have failed");
    assert_eq!(queue.len(), 64);

    // 3. Pop 1 item, then push 1 item (sliding window ring behavior)
    assert_eq!(queue.pop(), Some(0));
    assert_eq!(queue.len(), 63);
    assert_eq!(queue.push(1000), Ok(()));
    assert_eq!(queue.len(), 64);
    assert_eq!(queue.push(1001), Err(1001));

    // 4. Drain and verify FIFO ordering
    for expected in 1..64 {
        assert_eq!(queue.pop(), Some(expected));
    }
    assert_eq!(queue.pop(), Some(1000));
    assert_eq!(queue.pop(), None);
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);
}

#[test]
fn test_spsc_queue_extreme_contention_single_producer_single_consumer() {
    let mut queue = SpscQueue::<u64, 64>::new();
    let total_items = 200_000u64;
    thread::scope(|scope| {
        let (mut producer, mut consumer) = queue.split();
        let writer = scope.spawn(move || {
            for item in 0..total_items {
                while producer.push(item).is_err() {
                    std::hint::spin_loop();
                }
            }
        });
        let reader = scope.spawn(move || {
            for expected in 0..total_items {
                loop {
                    if let Some(item) = consumer.pop() {
                        assert_eq!(item, expected, "queue corrupted or reordered data");
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
        });
        writer.join().unwrap();
        reader.join().unwrap();
    });
    assert!(queue.is_empty());
}

#[test]
fn test_engine_packet_queue_saturation_via_c_abi() {
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

    let packet = [0x55u8; 128];

    // tx_packet_queue capacity is 64
    for i in 0..64 {
        let res = unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32) };
        assert_eq!(res, VRADM_OK, "Packet write {} failed", i);
    }

    // 65th packet must return VRADM_ERR_QUEUE_FULL
    let full_res = unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32) };
    assert_eq!(
        full_res, VRADM_ERR_QUEUE_FULL,
        "Queue saturation must return ERR_QUEUE_FULL"
    );

    // Command queue capacity is 32
    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 1,
        param_u32: 2,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };

    for i in 0..32 {
        let res = unsafe { vradm_submit_cmd(engine, &cmd) };
        assert_eq!(res, VRADM_OK, "Command submit {} failed", i);
    }

    // 33rd command must return VRADM_ERR_QUEUE_FULL
    let cmd_full_res = unsafe { vradm_submit_cmd(engine, &cmd) };
    assert_eq!(
        cmd_full_res, VRADM_ERR_QUEUE_FULL,
        "Command queue saturation must return ERR_QUEUE_FULL"
    );

    // Reset clears both queues
    unsafe { vradm_reset(engine) };

    // After reset, writing succeeds again
    assert_eq!(
        unsafe { vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32) },
        VRADM_OK
    );
    assert_eq!(unsafe { vradm_submit_cmd(engine, &cmd) }, VRADM_OK);

    unsafe { vradm_destroy(engine) };
}

#[test]
fn test_spsc_queue_reuses_ring_slots() {
    // Exercise repeated ring-slot reuse; engine unit tests cover usize overflow.
    let mut queue: SpscQueue<u32, 4> = SpscQueue::new();

    // Push and pop 10,000 items in small capacity queue to verify ring indexing
    for i in 0..10_000u32 {
        assert_eq!(queue.push(i), Ok(()));
        assert_eq!(queue.pop(), Some(i));
    }
    assert!(queue.is_empty());
}
