use vradm_core::{
    arq::IpPacketSlicer,
    c_abi::*,
    engine::vradm_engine,
    framing::CanonicalDataFrame,
    phy::{PhyReceiver, PhyTransmitter},
    security::*,
    session::*,
};

struct Fixed(u8);
impl NonceSource for Fixed {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        Ok([self.0; 16])
    }
}
fn transmitted(event: SessionEvent) -> BootstrapWire {
    match event {
        SessionEvent::Transmit(wire) => wire,
        _ => panic!("expected transmission"),
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
fn command(kind: u32) -> vradm_cmd_t {
    vradm_cmd_t {
        cmd_type: kind,
        cmd_id: 0,
        param_u32: 0,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    }
}
fn sessions() -> (SessionTransfer, SessionTransfer, Beacon) {
    sessions_seed(2)
}
fn sessions_seed(seed: u8) -> (SessionTransfer, SessionTransfer, Beacon) {
    let mut a = SessionManager::<8>::new([1; 16], 0);
    let mut b = SessionManager::<8>::new([1; 16], 0);
    let request = transmitted(a.begin(0, BootstrapMode::Fsk100, &mut Fixed(seed)).unwrap());
    let reply = transmitted(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Fixed(seed + 1))
            .unwrap(),
    );
    a.receive(&reply, 0, BootstrapMode::Fsk100, &mut Fixed(4))
        .unwrap();
    let confirmation = a.beacon(3, 3, 0, 0).unwrap();
    // Confirm the responder through real PHY decoding, before moving its
    // counter owner. Automatic host/audio handshake scheduling is separate work.
    let mut frame = CanonicalDataFrame::new();
    frame.ctrl = 0x3e;
    let mut tx = PhyTransmitter::new(3);
    let mut rx = PhyReceiver::new();
    let pcm = tx
        .modulate_authenticated(confirmation, &[frame], true)
        .unwrap();
    let mut frames = [CanonicalDataFrame::new(); 8];
    for chunk in pcm.chunks(160) {
        rx.ingest_samples(chunk);
        rx.process_with_verifier(&mut frames, true, &mut |beacon| {
            b.verify_peer_beacon(beacon, 0).is_ok()
        });
    }
    assert!(b.is_established(0).unwrap());
    let a_transfer = a.take_established(0).unwrap();
    let b_transfer = b.take_established(0).unwrap();
    assert!(matches!(
        a.take_established(0),
        Err(SessionError::NotEstablished)
    ));
    assert_eq!(a.beacon(3, 3, 0, 0), Err(SessionError::NotEstablished));
    assert_eq!(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Fixed(8)),
        Err(SessionError::Replay)
    );
    (a_transfer, b_transfer, confirmation)
}
fn packet_frame(mcs: u8) -> CanonicalDataFrame {
    let mut frames = [CanonicalDataFrame::new(); 8];
    IpPacketSlicer::slice_into(&[0x45; 19], false, false, 0, &mut frames).unwrap();
    frames[0].ctrl = (frames[0].ctrl & !0x78) | (mcs << 4) | 8;
    frames[0]
}

#[test]
fn authenticated_engines_transfer_both_directions_in_mcs2_and_mcs3() {
    for mcs in [2, 3] {
        let (a_session, b_session, _) = sessions();
        let mut a = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let mut b = vradm_engine::new_authenticated(config(mcs)).unwrap();
        let (mut ah, mut aa) = a.split();
        let (mut bh, mut ba) = b.split();
        assert!(ah.install_session(a_session).is_ok());
        assert!(bh.install_session(b_session).is_ok());
        assert!(!ah.authenticated_ready());
        assert_eq!(ah.write_ip_packet(&[0xa1; 19]), VRADM_OK);
        assert_eq!(bh.write_ip_packet(&[0xb1; 19]), VRADM_OK);
        let mut ab = [0; 160];
        let mut back = [0; 160];
        let mut out = [0; 296];
        let mut got_a = false;
        let mut got_b = false;
        for _ in 0..1000 {
            aa.generate_audio(&mut ab);
            ba.process_audio(&ab);
            ba.generate_audio(&mut back);
            aa.process_audio(&back);
            if ah.poll_ip_packet(&mut out) > 0 {
                assert_eq!(&out[..19], &[0xb1; 19]);
                assert!(!got_a);
                got_a = true;
            }
            if bh.poll_ip_packet(&mut out) > 0 {
                assert_eq!(&out[..19], &[0xa1; 19]);
                assert!(!got_b);
                got_b = true;
            }
        }
        assert!(got_a && got_b);
        assert!(ah.authenticated_ready() && bh.authenticated_ready());
    }
}

#[test]
fn authenticated_engine_is_silent_and_drops_input_until_install_and_after_reset() {
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert_eq!(host.write_ip_packet(&[9; 19]), VRADM_OK);
    let mut out = [123; 160];
    audio.generate_audio(&mut out);
    assert_eq!(out, [0; 160]);
    let mut legacy = PhyTransmitter::new(3);
    for chunk in legacy.modulate_burst(3, &[packet_frame(3)], 0).chunks(160) {
        audio.process_audio(chunk);
    }
    assert_eq!(host.poll_ip_packet(&mut [0; 296]), 0);
    let (session, _, _) = sessions();
    assert!(host.install_session(session).is_ok());
    audio.generate_audio(&mut out);
    assert!(host.authenticated_ready());
    assert_eq!(out, [0; 160], "pre-install generation must be discarded");
    assert_eq!(host.submit_cmd(&command(VRADM_CMD_RESET_SESSION)), VRADM_OK);
    audio.generate_audio(&mut out);
    assert!(!host.authenticated_ready());
    assert_eq!(out, [0; 160]);
}

#[test]
fn forged_header_cannot_deliver_data_and_replay_window_survives_handoff() {
    let (_, session, replay) = sessions();
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host.install_session(session).is_ok());
    audio.generate_audio(&mut [0; 160]);
    let mut tx = PhyTransmitter::new(3);
    // This counter was verified before handoff; replacing its payload must not
    // turn a replayed header into accepted data after transfer.
    for chunk in tx
        .modulate_authenticated(replay, &[packet_frame(3)], true)
        .unwrap()
        .chunks(160)
    {
        audio.process_audio(chunk);
    }
    assert_eq!(host.poll_ip_packet(&mut [0; 296]), 0);
    let mut signer = ControlTx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]));
    signer.beacon(3, 3, 0).unwrap();
    let valid = signer.beacon(3, 3, 0).unwrap();
    let bad = Beacon {
        mac: valid.mac ^ 1,
        ..valid
    };
    for chunk in tx
        .modulate_authenticated(bad, &[packet_frame(3)], true)
        .unwrap()
        .chunks(160)
    {
        audio.process_audio(chunk);
    }
    assert_eq!(host.poll_ip_packet(&mut [0; 296]), 0);
    // Same counter remains admissible after the forged version was rejected.
    for chunk in tx
        .modulate_authenticated(valid, &[packet_frame(3)], true)
        .unwrap()
        .chunks(160)
    {
        audio.process_audio(chunk);
    }
    assert_eq!(host.poll_ip_packet(&mut [0; 296]), 19);
    let mut telemetry = unsafe { core::mem::zeroed() };
    host.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.security_tamper_detected, 1);
    assert_eq!(telemetry.frames_received, 1);
}

#[test]
fn queue_full_returns_single_use_session_for_retry() {
    let (session, _, _) = sessions();
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    for _ in 0..32 {
        assert_eq!(host.submit_cmd(&command(VRADM_CMD_NONE)), VRADM_OK);
    }
    let session = match host.install_session(session) {
        Err((code, returned)) => {
            assert_eq!(code, VRADM_ERR_QUEUE_FULL);
            returned
        }
        Ok(()) => panic!("queue should be full"),
    };
    assert!(!host.authenticated_ready());
    audio.generate_audio(&mut [0; 160]);
    assert!(host.install_session(session).is_ok());
    audio.generate_audio(&mut [0; 160]);
    assert!(host.authenticated_ready());
}

#[test]
fn unsupported_authenticated_profiles_are_rejected() {
    for mcs in [0, 1, 4] {
        assert!(vradm_engine::new_authenticated(config(mcs)).is_err());
    }
    let mut cfg = config(3);
    cfg.sample_rate = VRADM_RATE_16K;
    assert!(vradm_engine::new_authenticated(cfg).is_err());
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, _) = engine.split();
    let mut cmd = command(VRADM_CMD_REQUEST_MCS);
    cmd.param_u32 = 4;
    assert_eq!(host.submit_cmd(&cmd), VRADM_ERR_INVALID_ARG);
}

#[test]
fn session_replacement_waits_for_active_burst_and_preserves_later_commands() {
    let (first, _, _) = sessions();
    let (second, _, _) = sessions_seed(8);
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host.install_session(first).is_ok());
    assert_eq!(host.write_ip_packet(&[0x41; 19]), VRADM_OK);
    let mut out = [0; 160];
    audio.generate_audio(&mut out);
    assert!(host.authenticated_ready());
    assert!(host.install_session(second).is_ok());
    let mut change = command(VRADM_CMD_REQUEST_MCS);
    change.param_u32 = 2;
    assert_eq!(host.submit_cmd(&change), VRADM_OK);
    assert_eq!(host.write_ip_packet(&[0x42; 19]), VRADM_OK);
    for _ in 0..37 {
        audio.generate_audio(&mut out);
        assert!(!host.authenticated_ready());
        assert_eq!(host.get_active_mcs(), 3);
    }
    audio.generate_audio(&mut out);
    assert!(host.authenticated_ready());
    assert_eq!(host.get_active_mcs(), 2);
    assert!(out.iter().any(|&sample| sample != 0));
}

#[test]
fn lost_authenticated_burst_retransmits_under_a_fresh_beacon_counter() {
    let (a_session, b_session, _) = sessions();
    let mut a = vradm_engine::new_authenticated(config(3)).unwrap();
    let mut b = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut ah, mut aa) = a.split();
    let (mut bh, mut ba) = b.split();
    assert!(ah.install_session(a_session).is_ok());
    assert!(bh.install_session(b_session).is_ok());
    ah.write_ip_packet(&[0x61; 19]);
    let mut ab = [0; 160];
    let mut back = [0; 160];
    let mut received = 0;
    for tick in 0..1000 {
        aa.generate_audio(&mut ab);
        if tick < 38 {
            ab.fill(0);
        } // erase the entire initial 5,960-sample burst
        ba.process_audio(&ab);
        ba.generate_audio(&mut back);
        aa.process_audio(&back);
        let mut packet = [0; 296];
        if bh.poll_ip_packet(&mut packet) > 0 {
            assert_eq!(&packet[..19], &[0x61; 19]);
            received += 1;
        }
    }
    assert_eq!(received, 1);
    let mut telemetry = unsafe { core::mem::zeroed() };
    bh.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.security_tamper_detected, 0);
}

#[test]
fn exhausted_transferred_counter_stops_engine_without_unauthenticated_fallback() {
    let mut manager = SessionManager::<8>::new([1; 16], 0);
    let request = transmitted(
        manager
            .begin(0, BootstrapMode::Fsk100, &mut Fixed(2))
            .unwrap(),
    );
    let verified = VerifiedRequest::decode(&request, &[1; 16]).unwrap();
    let (reply, _) = verified.accept(&[1; 16], [3; 16]);
    manager
        .receive(&reply, 0, BootstrapMode::Fsk100, &mut Fixed(4))
        .unwrap();
    for _ in 0..REKEY_COUNTER {
        manager.beacon(3, 3, 0, 0).unwrap();
    }
    let mut engine = vradm_engine::new_authenticated(config(3)).unwrap();
    let (mut host, mut audio) = engine.split();
    assert!(host
        .install_session(manager.take_established(0).unwrap())
        .is_ok());
    host.write_ip_packet(&[0x62; 19]);
    let mut out = [123; 160];
    audio.generate_audio(&mut out);
    assert_eq!(out, [0; 160]);
    assert!(!host.authenticated_ready());
    audio.generate_audio(&mut out);
    assert_eq!(out, [0; 160]);
}
