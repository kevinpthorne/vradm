use vradm_core::{
    c_abi::*,
    engine::vradm_engine,
    handshake::{HandshakeCoordinator, HandshakePhase},
    handshake_bridge::*,
    session::{NonceSource, SessionError},
};
struct Source(u8);
impl NonceSource for Source {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        let result = [self.0; 16];
        self.0 += 1;
        Ok(result)
    }
}
fn config(mcs: u8) -> vradm_config_t {
    vradm_config_t {
        sample_rate: VRADM_RATE_8K,
        startup_mcs: mcs,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.1334,
        reserved2: [0; 4],
        psk_key: [1; 16],
    }
}

#[test]
fn playback_fence_requires_device_ack_after_partial_blocks() {
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    assert_eq!(host.push_playback(&[]), Err(BridgeError::InvalidLength));
    assert_eq!(
        host.push_playback(&[0; 161]),
        Err(BridgeError::InvalidLength)
    );
    host.push_playback(&[1; 160]).unwrap();
    host.push_playback(&[2; 17]).unwrap();
    host.seal_playback().unwrap();
    assert_eq!(host.push_playback(&[3]), Err(BridgeError::Sealed));
    let mut first = [0; 159];
    assert_eq!(
        audio.render(&mut first),
        PlaybackProgress {
            samples: 159,
            awaiting_device_drain: false
        }
    );
    assert_eq!(first, [1; 159]);
    assert!(audio.playback_fence().is_none());
    let mut last = [9; 80];
    assert_eq!(
        audio.render(&mut last),
        PlaybackProgress {
            samples: 18,
            awaiting_device_drain: true
        }
    );
    assert_eq!(last[0], 1);
    assert_eq!(&last[1..18], &[2; 17]);
    assert_eq!(&last[18..], &[0; 62]);
    assert!(!host.playback_drained());
    let fence = audio.playback_fence().unwrap();
    assert!(audio.acknowledge_played(fence));
    assert!(host.playback_drained());
}

#[test]
fn queue_saturation_preserves_playback_and_marks_capture_gap() {
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    for i in 0..PCM_QUEUE_BLOCKS {
        host.push_playback(&[i as i16; 160]).unwrap();
    }
    assert_eq!(host.push_playback(&[99]), Err(BridgeError::Full));
    assert_eq!(host.seal_playback(), Err(BridgeError::Full));
    let mut out = [0; 160];
    for i in 0..PCM_QUEUE_BLOCKS {
        assert_eq!(audio.render(&mut out).samples, 160);
        assert_eq!(out, [i as i16; 160]);
    }
    host.seal_playback().unwrap();
    assert!(audio.render(&mut out).awaiting_device_drain);
    assert_eq!(out, [0; 160]);

    for _ in 0..PCM_QUEUE_BLOCKS {
        assert_eq!(audio.capture(&[1; 160]), 0);
    }
    assert_eq!(audio.capture(&[9; 320]), 320);
    for _ in 0..PCM_QUEUE_BLOCKS {
        assert!(!host.pop_capture().unwrap().discontinuity);
    }
    assert_eq!(audio.capture(&[2; 17]), 0);
    let after_gap = host.pop_capture().unwrap();
    assert!(after_gap.discontinuity);
    assert_eq!(after_gap.len, 17);
    audio.capture(&[3]);
    assert!(!host.pop_capture().unwrap().discontinuity);
    audio.capture_discontinuity();
    audio.capture(&[4]);
    assert!(host.pop_capture().unwrap().discontinuity);
}

#[test]
fn reset_discards_partial_playback_old_capture_and_stale_fence() {
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    host.push_playback(&[1; 160]).unwrap();
    host.seal_playback().unwrap();
    audio.capture(&[1; 160]);
    let mut prefix = [0; 7];
    audio.render(&mut prefix);
    host.reset();
    assert!(audio.playback_fence().is_none());
    assert!(!host.playback_drained());
    assert!(host.pop_capture().is_none());
    host.push_playback(&[2; 13]).unwrap();
    host.seal_playback().unwrap();
    let mut out = [7; 320];
    assert_eq!(
        audio.render(&mut out),
        PlaybackProgress {
            samples: 13,
            awaiting_device_drain: true
        }
    );
    assert_eq!(&out[..13], &[2; 13]);
    assert!(out[13..].iter().all(|&s| s == 0));
    host.reset();
    assert!(audio.playback_fence().is_none());
    audio.capture(&[3; 160]);
    assert!(!host.pop_capture().unwrap().discontinuity);
}

#[test]
fn bridged_handshake_drains_before_install_and_routes_engine_pcm() {
    for mcs in [2, 3] {
        let mut ab = HandshakeBridge::new();
        let mut bb = HandshakeBridge::new();
        let (ahb, mut aa) = ab.split();
        let (bhb, mut ba) = bb.split();
        let mut aw = HandshakeWorker::new(
            HandshakeCoordinator::<16>::new([1; 16], mcs, 0).unwrap(),
            ahb,
        );
        let mut bw = HandshakeWorker::new(
            HandshakeCoordinator::<16>::new([1; 16], mcs, 0).unwrap(),
            bhb,
        );
        let mut ae = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let mut be = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let (mut ah, mut ad) = ae.split();
        let (mut bh, mut bd) = be.split();
        let mut a_source = Source(2);
        let mut b_source = Source(20);
        aw.begin(0, &mut a_source).unwrap();
        let mut a_pcm = [0; 160];
        let mut b_pcm = [0; 160];
        let mut a_sent = false;
        let mut b_sent = false;
        let mut fence_seen = false;
        for step in 0..1600 {
            let now = step * 20;
            aw.pump(now, &mut a_source).unwrap();
            bw.pump(now, &mut b_source).unwrap();
            let ap = aa.generate_audio(&mut ad, &mut a_pcm);
            let bp = ba.generate_audio(&mut bd, &mut b_pcm);
            assert_eq!(aa.process_audio(&mut ad, &b_pcm), 0);
            assert_eq!(ba.process_audio(&mut bd, &a_pcm), 0);
            // In this synchronous test the returned PCM has just reached the peer.
            // Real devices acknowledge after their downstream queue drains.
            if ap.awaiting_device_drain && !aa.routes_to_engine() {
                assert!(!aw.install_when_drained(&mut ah, now).unwrap());
                assert!(!aa.routes_to_engine());
                let fence = aa.playback_fence().unwrap();
                assert!(aa.acknowledge_played(fence));
                fence_seen = true;
            }
            if bp.awaiting_device_drain && !ba.routes_to_engine() {
                let fence = ba.playback_fence().unwrap();
                ba.acknowledge_played(fence);
            }
            if aw.install_when_drained(&mut ah, now).unwrap() && !a_sent {
                assert_eq!(ah.write_ip_packet(&[0x45; 19]), VRADM_OK);
                a_sent = true;
            }
            if bw.install_when_drained(&mut bh, now).unwrap() && !b_sent {
                assert_eq!(bh.write_ip_packet(&[0x46; 19]), VRADM_OK);
                b_sent = true;
            }
        }
        assert!(fence_seen && a_sent && b_sent);
        let mut packet = [0; 2048];
        assert_eq!(ah.poll_ip_packet(&mut packet), 19);
        assert_eq!(&packet[..19], &[0x46; 19]);
        assert_eq!(bh.poll_ip_packet(&mut packet), 19);
        assert_eq!(&packet[..19], &[0x45; 19]);
        assert_eq!(aw.phase(), HandshakePhase::Transferred);
        assert_eq!(bw.phase(), HandshakePhase::Transferred);
    }
}

#[test]
fn stalled_worker_keeps_render_and_capture_bounded() {
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    let mut out = [1; 511];
    assert_eq!(audio.render(&mut out).samples, 0);
    assert_eq!(out, [0; 511]);
    for _ in 0..100 {
        audio.capture(&[7; 511]);
    }
    let mut count = 0;
    while host.pop_capture().is_some() {
        count += 1;
    }
    assert_eq!(count, PCM_QUEUE_BLOCKS);
}

#[test]
fn engine_queue_full_retains_transfer_and_never_enables_routing_early() {
    let mut bridge = HandshakeBridge::new();
    let (host, mut audio) = bridge.split();
    let mut worker = HandshakeWorker::new(
        HandshakeCoordinator::<16>::new([1; 16], 3, 0).unwrap(),
        host,
    );
    let mut peer = HandshakeCoordinator::<16>::new([1; 16], 3, 0).unwrap();
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut eh, mut ea) = engine.split();
    let command = vradm_cmd_t {
        cmd_type: VRADM_CMD_NONE,
        cmd_id: 0,
        param_u32: 0,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };
    for _ in 0..32 {
        assert_eq!(eh.submit_cmd(&command), VRADM_OK);
    }
    let mut source = Source(2);
    worker.begin(0, &mut source).unwrap();
    let mut out = [0; 160];
    let mut incoming = [0; 160];
    let mut now = 0;
    for step in 0..700 {
        now = step * 20;
        worker.pump(now, &mut source).unwrap();
        let progress = audio.render(&mut out);
        peer.render_pcm(&mut incoming, now).unwrap();
        peer.process_pcm(&out, now, &mut Source(20)).unwrap();
        audio.capture(&incoming);
        if progress.awaiting_device_drain {
            let fence = audio.playback_fence().unwrap();
            audio.acknowledge_played(fence);
            break;
        }
    }
    assert!(worker.playback_drained());
    assert_eq!(
        worker.install_when_drained(&mut eh, now),
        Err(WorkerError::Engine(VRADM_ERR_QUEUE_FULL))
    );
    assert!(!audio.routes_to_engine());
    assert!(!eh.authenticated_ready());
    worker.pump(now, &mut source).unwrap(); // Transfer ownership stays in worker.
    audio.generate_audio(&mut ea, &mut out); // Wrapper services commands while gated.
    assert_eq!(out, [0; 160]);
    assert_eq!(worker.install_when_drained(&mut eh, now), Ok(true));
    assert!(audio.routes_to_engine());
    assert!(!eh.authenticated_ready()); // Enqueued, not yet applied by audio owner.
    audio.generate_audio(&mut ea, &mut out);
    assert!(eh.authenticated_ready());
    assert_eq!(worker.install_when_drained(&mut eh, now), Ok(true));
}

#[test]
fn concurrent_endpoints_preserve_order_under_backpressure() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut bridge = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let done_ref = &done;
        let output = scope.spawn(move || {
            let mut received = Vec::new();
            let mut out = [0; 113];
            loop {
                let progress = audio.render(&mut out);
                received.extend_from_slice(&out[..progress.samples]);
                audio.capture(&out[..progress.samples]);
                if progress.awaiting_device_drain {
                    let fence = audio.playback_fence().unwrap();
                    audio.acknowledge_played(fence);
                    break;
                }
                std::thread::yield_now();
            }
            done_ref.store(true, Ordering::Release);
            received
        });
        for number in 0..1000 {
            loop {
                while host.pop_capture().is_some() {}
                if host.push_playback(&[number; 37]).is_ok() {
                    break;
                }
                std::thread::yield_now();
            }
        }
        while host.seal_playback() == Err(BridgeError::Full) {
            while host.pop_capture().is_some() {}
            std::thread::yield_now();
        }
        while !done.load(Ordering::Acquire) {
            while host.pop_capture().is_some() {}
            std::thread::yield_now();
        }
        assert!(host.playback_drained());
        let samples = output.join().unwrap();
        assert_eq!(samples.len(), 37000);
        for (number, chunk) in samples.chunks(37).enumerate() {
            assert_eq!(chunk, &[number as i16; 37]);
        }
    });
}

#[test]
fn worker_timeout_invalidates_pre_rendered_request_and_pending_block() {
    let mut bridge = HandshakeBridge::new();
    let (host, mut audio) = bridge.split();
    let mut worker = HandshakeWorker::new(
        HandshakeCoordinator::<16>::new([1; 16], 3, 0).unwrap(),
        host,
    );
    let mut source = Source(2);
    worker.begin(0, &mut source).unwrap();
    worker.pump(0, &mut source).unwrap(); // Full queue plus retained render block.
    let mut out = [0; 13];
    assert_eq!(audio.render(&mut out).samples, 13);
    assert_eq!(
        worker.pump(42000, &mut source),
        Ok(HandshakePhase::TimedOut)
    );
    let mut silence = [9; 320];
    assert_eq!(audio.render(&mut silence).samples, 0);
    assert_eq!(silence, [0; 320]);
    assert!(audio.playback_fence().is_none());
    worker.reset(42000).unwrap();
    worker.begin(42000, &mut source).unwrap();
    worker.pump(42000, &mut source).unwrap();
    assert_eq!(audio.render(&mut silence).samples, 320);
}

#[test]
fn delayed_device_completion_cannot_ack_new_generation_or_other_bridge() {
    let mut bridge = HandshakeBridge::new();
    let mut other = HandshakeBridge::new();
    let (mut host, mut audio) = bridge.split();
    let (mut other_host, mut other_audio) = other.split();
    let mut out = [0; 160];
    host.seal_playback().unwrap();
    audio.render(&mut out);
    let old = audio.playback_fence().unwrap();
    host.reset();
    host.seal_playback().unwrap();
    audio.render(&mut out);
    let current = audio.playback_fence().unwrap();
    assert!(!audio.acknowledge_played(old));
    assert!(!host.playback_drained());
    other_host.seal_playback().unwrap();
    other_audio.render(&mut out);
    assert!(!other_audio.acknowledge_played(current));
    assert!(!other_host.playback_drained());
    assert!(audio.acknowledge_played(current));
    assert!(host.playback_drained());
}
