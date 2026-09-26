use vradm_core::{
    bootstrap_phy::{BarkerBootstrapTransmitter, BARKER_BOOTSTRAP_SAMPLES},
    c_abi::*,
    engine::vradm_engine,
    handshake::*,
    security::PendingBootstrap,
    session::*,
};

struct Source(u8);
impl NonceSource for Source {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        let nonce = [self.0; 16];
        self.0 += 1;
        Ok(nonce)
    }
}
struct Unavailable;
impl NonceSource for Unavailable {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        Err(SessionError::EntropyUnavailable)
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
fn pair(mcs: u8) -> (HandshakeCoordinator<16>, HandshakeCoordinator<16>) {
    (
        HandshakeCoordinator::new([1; 16], mcs, 0).unwrap(),
        HandshakeCoordinator::new([1; 16], mcs, 0).unwrap(),
    )
}

fn exchange(
    a: &mut HandshakeCoordinator<16>,
    b: &mut HandshakeCoordinator<16>,
    chunk: usize,
    lose_accept: bool,
) -> u64 {
    let mut ab = vec![0; chunk];
    let mut ba = vec![0; chunk];
    for n in 0..(30 * 8000 / chunk) {
        let now = (n * chunk / 8) as u64;
        a.render_pcm(&mut ab, now).unwrap();
        b.render_pcm(&mut ba, now).unwrap();
        if lose_accept && now < 6000 {
            ba.fill(0);
        }
        a.process_pcm(&ba, now, &mut Source(10)).unwrap();
        b.process_pcm(&ab, now, &mut Source(20)).unwrap();
        if a.phase() == HandshakePhase::Ready && b.phase() == HandshakePhase::Ready {
            return now;
        }
    }
    panic!("handshake stalled: {:?} {:?}", a.phase(), b.phase());
}

#[test]
fn pcm_handshake_hands_counter_owners_to_bidirectional_data_engines() {
    for (mcs, chunk) in [(2, 80), (3, 511)] {
        let (mut a, mut b) = pair(mcs);
        a.begin(0, &mut Source(2)).unwrap();
        let now = exchange(&mut a, &mut b, chunk, false);
        let mut ae = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let mut be = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let (mut ah, mut aa) = ae.split();
        let (mut bh, mut ba) = be.split();
        assert!(ah.install_session(a.take_established(now).unwrap()).is_ok());
        assert!(bh.install_session(b.take_established(now).unwrap()).is_ok());
        assert!(matches!(
            a.take_established(now),
            Err(HandshakeError::NotReady)
        ));
        assert_eq!(ah.write_ip_packet(&[0x45; 19]), VRADM_OK);
        assert_eq!(bh.write_ip_packet(&[0x46; 19]), VRADM_OK);
        let mut ab = [0; 160];
        let mut bb = [0; 160];
        for _ in 0..1000 {
            aa.generate_audio(&mut ab);
            ba.generate_audio(&mut bb);
            aa.process_audio(&bb);
            ba.process_audio(&ab);
        }
        let mut packet = [0; 2048];
        assert_eq!(ah.poll_ip_packet(&mut packet), 19);
        assert_eq!(&packet[..19], &[0x46; 19]);
        assert_eq!(bh.poll_ip_packet(&mut packet), 19);
        assert_eq!(&packet[..19], &[0x45; 19]);
    }
}

#[test]
fn lost_accept_is_recovered_by_automatic_request_retry() {
    let (mut a, mut b) = pair(3);
    a.begin(0, &mut Source(2)).unwrap();
    let now = exchange(&mut a, &mut b, 160, true);
    assert!(now >= 6000);
    assert_eq!(a.bootstrap_mac_failures(), 0);
    assert_eq!(b.bootstrap_mac_failures(), 0);
}

#[test]
fn simultaneous_initiators_resolve_roles_without_device_labels() {
    let (mut a, mut b) = pair(3);
    a.begin(0, &mut Source(2)).unwrap();
    b.begin(0, &mut Source(5)).unwrap();
    let now = exchange(&mut a, &mut b, 160, false);
    assert!(a.take_established(now).is_ok());
    assert!(b.take_established(now).is_ok());
}

#[test]
fn malformed_or_wrong_psk_pcm_never_releases_data_gate() {
    let (_, mut b) = pair(3);
    let request = PendingBootstrap::new([9; 16], [2; 16]).request().unwrap();
    let mut pcm = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    BarkerBootstrapTransmitter::new(request).render(&mut pcm);
    // A large caller input is internally chunked and fully consumed.
    b.process_pcm(&pcm, 3000, &mut Unavailable).unwrap();
    assert_eq!(b.bootstrap_mac_failures(), 1);
    assert_eq!(b.rejected_bootstrap_frames(), 1);
    assert_eq!(b.phase(), HandshakePhase::Listening);
    assert!(matches!(
        b.take_established(3000),
        Err(HandshakeError::NotReady)
    ));
    let mut out = [9; 160];
    assert_eq!(b.render_pcm(&mut out, 3000), Ok(0));
    assert_eq!(out, [0; 160]);
}

#[test]
fn timeout_reset_clock_and_entropy_errors_leave_reviewable_state() {
    assert!(matches!(
        HandshakeCoordinator::<8>::new([1; 16], 4, 0),
        Err(HandshakeError::UnsupportedMcs)
    ));
    let (mut a, _) = pair(3);
    assert_eq!(
        a.begin(0, &mut Unavailable),
        Err(HandshakeError::Session(SessionError::EntropyUnavailable))
    );
    assert_eq!(a.phase(), HandshakePhase::Listening);
    a.begin(0, &mut Source(2)).unwrap();
    assert_eq!(a.begin(0, &mut Source(3)), Err(HandshakeError::Busy));
    assert_eq!(a.poll(42000), Ok(HandshakePhase::TimedOut));
    let mut out = [1; 160];
    assert_eq!(a.render_pcm(&mut out, 42000), Ok(0));
    assert_eq!(out, [0; 160]);
    assert_eq!(
        a.reset(41999),
        Err(HandshakeError::Session(SessionError::ClockWentBackwards))
    );
    a.reset(42000).unwrap();
    assert_eq!(
        a.begin(42000, &mut Source(2)),
        Err(HandshakeError::Session(SessionError::NonceCollision))
    );
    a.begin(42000, &mut Source(3)).unwrap();
}

#[test]
fn initiator_cannot_transfer_before_confirmation_is_rendered() {
    let (mut a, mut b) = pair(3);
    a.begin(0, &mut Source(2)).unwrap();
    let mut request = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    assert_eq!(a.render_pcm(&mut request, 0).unwrap(), request.len());
    b.process_pcm(&request, 2625, &mut Source(3)).unwrap();
    let mut reply = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    b.render_pcm(&mut reply, 2625).unwrap();
    a.process_pcm(&reply, 5250, &mut Unavailable).unwrap();
    assert_eq!(a.phase(), HandshakePhase::Negotiating);
    assert!(matches!(
        a.take_established(5250),
        Err(HandshakeError::NotReady)
    ));
    let mut first = [0; 1];
    a.render_pcm(&mut first, 5250).unwrap();
    assert!(matches!(
        a.take_established(5250),
        Err(HandshakeError::NotReady)
    ));
    let mut tail = vec![0; 10000];
    a.render_pcm(&mut tail, 5251).unwrap();
    assert_eq!(a.phase(), HandshakePhase::Ready);
    b.process_pcm(&first, 5251, &mut Unavailable).unwrap();
    b.process_pcm(&tail, 6500, &mut Unavailable).unwrap();
    assert_eq!(b.phase(), HandshakePhase::Ready);
}

// Establish A through accept PCM, but leave B waiting for the first peer PLCP.
fn accepted(mcs: u8) -> (HandshakeCoordinator<16>, HandshakeCoordinator<16>, Vec<i16>) {
    let (mut a, mut b) = pair(mcs);
    a.begin(0, &mut Source(2)).unwrap();
    let mut request = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    a.render_pcm(&mut request, 0).unwrap();
    b.process_pcm(&request, 2625, &mut Source(3)).unwrap();
    let mut reply = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    b.render_pcm(&mut reply, 2625).unwrap();
    a.process_pcm(&reply, 5250, &mut Unavailable).unwrap();
    let mut confirmation = vec![0; 10000];
    let count = a.render_pcm(&mut confirmation, 5250).unwrap();
    confirmation.truncate(count);
    assert_eq!(a.phase(), HandshakePhase::Ready);
    assert_eq!(b.phase(), HandshakePhase::Negotiating);
    (a, b, confirmation)
}

#[test]
fn lost_confirmation_recovers_on_reliable_data_retry_after_receiver_handoff() {
    for mcs in [2, 3] {
        let (mut a, mut b, _lost_confirmation) = accepted(mcs);
        let mut ae = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let mut be = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let (mut ah, mut aa) = ae.split();
        let (mut bh, mut ba) = be.split();
        assert!(ah
            .install_session(a.take_established(6500).unwrap())
            .is_ok());
        assert_eq!(ah.write_ip_packet(&[0x45; 19]), VRADM_OK);
        let mut ab = [0; 160];
        let mut bb = [0; 160];
        let mut installed = false;
        for step in 0..1500 {
            let now = 6500 + step * 20;
            aa.generate_audio(&mut ab);
            ba.generate_audio(&mut bb);
            aa.process_audio(&bb);
            if installed {
                ba.process_audio(&ab);
            } else {
                b.process_pcm(&ab, now, &mut Unavailable).unwrap();
                if b.phase() == HandshakePhase::Ready {
                    assert!(bh.install_session(b.take_established(now).unwrap()).is_ok());
                    installed = true;
                }
            }
        }
        assert!(installed);
        let mut out = [0; 2048];
        assert_eq!(bh.poll_ip_packet(&mut out), 19);
        assert_eq!(&out[..19], &[0x45; 19]);
        assert_eq!(bh.poll_ip_packet(&mut out), 0);
    }
}

#[test]
fn forged_confirmation_cannot_release_provisional_responder() {
    use vradm_core::{
        framing::CanonicalDataFrame,
        phy::PhyTransmitter,
        security::{ControlTx, SessionKeys},
    };
    let (_, mut b, valid) = accepted(3);
    let mut tx = ControlTx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]));
    let mut beacon = tx.beacon(3, 3, 0).unwrap();
    beacon.mac ^= 1;
    let mut frame = CanonicalDataFrame::new();
    frame.ctrl = 0x3e;
    let mut phy = PhyTransmitter::new(3);
    let forged = phy.modulate_authenticated(beacon, &[frame], true).unwrap();
    b.process_pcm(forged, 6500, &mut Unavailable).unwrap();
    assert_eq!(b.phase(), HandshakePhase::Negotiating);
    assert!(matches!(
        b.take_established(6500),
        Err(HandshakeError::NotReady)
    ));
    // A forged counter must not consume the genuine confirmation's replay bit.
    b.process_pcm(&valid, 7500, &mut Unavailable).unwrap();
    assert_eq!(b.phase(), HandshakePhase::Ready);
    assert!(b.take_established(7500).is_ok());
}

#[test]
fn provisional_timeout_and_reset_discard_queued_accept_audio() {
    let (_, mut b) = pair(3);
    let request = PendingBootstrap::new([1; 16], [2; 16]).request().unwrap();
    let mut pcm = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    BarkerBootstrapTransmitter::new(request).render(&mut pcm);
    b.process_pcm(&pcm, 3000, &mut Source(3)).unwrap();
    let mut first = [0; 160];
    assert_eq!(b.render_pcm(&mut first, 3000), Ok(160));
    assert_eq!(b.poll(45000), Ok(HandshakePhase::TimedOut));
    assert_eq!(b.render_pcm(&mut first, 45000), Ok(0));
    assert_eq!(first, [0; 160]);
    b.reset(45000).unwrap();
    b.process_pcm(&pcm, 48000, &mut Unavailable).unwrap();
    assert_eq!(b.phase(), HandshakePhase::Listening);
    assert_eq!(b.render_pcm(&mut first, 48000), Ok(0));
}

#[test]
fn idle_confirmation_recovers_even_when_first_two_engine_probes_are_lost() {
    for mcs in [2, 3] {
        let (mut a, mut b, _lost_initial) = accepted(mcs);
        let mut engine = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let (mut host, mut audio) = engine.split();
        assert!(host
            .install_session(a.take_established(6500).unwrap())
            .is_ok());
        let mut pcm = [0; 160];
        for step in 0..1500 {
            audio.generate_audio(&mut pcm);
            if step < 300 {
                assert!(pcm.iter().all(|&sample| sample == 0));
            }
            // Lose the coordinator confirmation and the first two recovery bursts.
            if step < 850 {
                pcm.fill(0);
            }
            b.process_pcm(&pcm, 6500 + step * 20, &mut Unavailable)
                .unwrap();
        }
        assert_eq!(b.phase(), HandshakePhase::Ready);
        assert!(b.take_established(36500).is_ok());
        let mut telemetry = unsafe { core::mem::zeroed() };
        host.get_telemetry(&mut telemetry);
        assert_eq!(telemetry.frames_transmitted, 3);
        let mut packet = [0; 2048];
        assert_eq!(host.poll_ip_packet(&mut packet), 0);
    }
}

#[test]
fn confirmation_retries_use_fresh_counters_then_stop_without_ping_pong() {
    use vradm_core::{
        framing::CanonicalDataFrame,
        phy::PhyReceiver,
        security::{ControlRx, SessionKeys},
    };
    let (mut a, _, _) = accepted(3);
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host
        .install_session(a.take_established(6500).unwrap())
        .is_ok());
    let mut verifier = ControlRx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]), 6500);
    let mut receiver = PhyReceiver::new();
    let mut counters = Vec::new();
    let mut frames = [CanonicalDataFrame::new(); 8];
    let mut pcm = [0; 160];
    for step in 0..1800 {
        audio.generate_audio(&mut pcm);
        if step >= 1100 {
            assert!(pcm.iter().all(|&sample| sample == 0));
        }
        receiver.ingest_samples(&pcm);
        receiver.process_with_verifier(&mut frames, true, &mut |beacon| {
            let counter = verifier.verify_beacon(beacon, 6500 + step * 20).unwrap();
            counters.push(counter);
            true
        });
    }
    assert_eq!(counters, vec![1, 2, 3]); // Counter 0 was rendered by coordinator.
}

#[test]
fn verified_peer_cancels_retries_but_forged_peer_does_not() {
    use vradm_core::{
        framing::CanonicalDataFrame,
        phy::PhyTransmitter,
        security::{ControlTx, SessionKeys},
    };
    let (mut a, _, _) = accepted(3);
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host
        .install_session(a.take_established(6500).unwrap())
        .is_ok());
    let mut peer = ControlTx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]));
    let good = peer.beacon(3, 3, 0).unwrap();
    let mut forged = good;
    forged.mac ^= 1;
    let mut frame = CanonicalDataFrame::new();
    frame.ctrl = 0x3e;
    let mut phy = PhyTransmitter::new(3);
    for chunk in phy
        .modulate_authenticated(forged, &[frame], false)
        .unwrap()
        .chunks(160)
    {
        audio.process_audio(chunk);
    }
    let mut pcm = [0; 160];
    for _ in 0..400 {
        audio.generate_audio(&mut pcm);
    }
    let mut telemetry = unsafe { core::mem::zeroed() };
    host.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.security_tamper_detected, 1);
    assert_eq!(telemetry.frames_transmitted, 1);
    for chunk in phy
        .modulate_authenticated(good, &[frame], false)
        .unwrap()
        .chunks(160)
    {
        audio.process_audio(chunk);
    }
    for _ in 0..1200 {
        audio.generate_audio(&mut pcm);
        assert_eq!(pcm, [0; 160]);
    }
    host.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.frames_transmitted, 1);
}

#[test]
fn reset_cancels_armed_confirmation_retries() {
    let (mut a, _, _) = accepted(3);
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host
        .install_session(a.take_established(6500).unwrap())
        .is_ok());
    let mut pcm = [0; 160];
    for _ in 0..100 {
        audio.generate_audio(&mut pcm);
    }
    let cmd = vradm_cmd_t {
        cmd_type: VRADM_CMD_RESET_SESSION,
        cmd_id: 0,
        param_u32: 0,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };
    assert_eq!(host.submit_cmd(&cmd), VRADM_OK);
    for _ in 0..1500 {
        audio.generate_audio(&mut pcm);
        assert_eq!(pcm, [0; 160]);
    }
    assert!(!host.authenticated_ready());
}
