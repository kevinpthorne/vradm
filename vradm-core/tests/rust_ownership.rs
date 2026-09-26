use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use vradm_core::{
    c_abi::*,
    engine::{vradm_engine, SpscQueue},
};

#[test]
fn safe_host_audio_handles_transfer_packets_on_separate_threads() {
    let config = vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: VRADM_MCS_3,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.1334,
        reserved2: [0; 4],
        psk_key: [0x5a; 16],
    };
    let mut engine = vradm_engine::new(config);
    let stop = AtomicBool::new(false);
    let received = thread::scope(|scope| {
        let (mut host, mut audio) = engine.split();
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _stop_on_panic = StopOnDrop(&stop);
        let audio_stop = &stop;
        scope.spawn(move || {
            let mut samples = [0; 160];
            while !audio_stop.load(Ordering::Acquire) {
                audio.generate_audio(&mut samples);
                audio.process_audio(&samples);
            }
        });
        let packet = [0x71; 128];
        assert_eq!(host.write_ip_packet(&packet), VRADM_OK);
        let mut out = [0; 296];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = false;
        while Instant::now() < deadline {
            let len = host.poll_ip_packet(&mut out);
            if len > 0 {
                received = len == 128 && out[..128] == packet;
                break;
            }
            thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        received
    });
    assert!(received, "safe handles failed PCM loopback delivery");
    // Both handles have returned, so a synchronous reset is now legal.
    engine.reset_exclusive();
    assert_eq!(engine.tx_packet_queue.len(), 0);
    assert_eq!(engine.rx_packet_queue.len(), 0);
}

#[test]
fn queue_drops_owned_items_exactly_once() {
    struct CountDrop(Arc<AtomicUsize>);
    impl Drop for CountDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    {
        let mut queue = SpscQueue::<CountDrop, 4>::new();
        let (mut producer, mut consumer) = queue.split();
        for _ in 0..4 {
            assert!(producer.push(CountDrop(drops.clone())).is_ok());
        }
        let rejected = producer.push(CountDrop(drops.clone()));
        assert!(rejected.is_err());
        drop(rejected);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(consumer.peek().is_some());
        drop(consumer.pop());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        consumer.clear();
        assert_eq!(drops.load(Ordering::Relaxed), 5);
        assert!(producer.push(CountDrop(drops.clone())).is_ok());
        // Queued value is owned by the queue after its endpoints go away.
    }
    assert_eq!(drops.load(Ordering::Relaxed), 6);
}

#[test]
fn queue_moves_send_but_not_sync_values_between_owners() {
    use std::cell::Cell;
    let mut queue = SpscQueue::<Cell<u32>, 4>::new();
    thread::scope(|scope| {
        let (mut producer, mut consumer) = queue.split();
        scope
            .spawn(move || producer.push(Cell::new(42)).unwrap())
            .join()
            .unwrap();
        scope
            .spawn(move || {
                consumer.peek().unwrap().set(43);
                assert_eq!(consumer.pop().unwrap().get(), 43);
            })
            .join()
            .unwrap();
    });
}
