use vradm_core::{
    c_abi::*,
    endpoint::*,
    session::{NonceSource, SessionError},
};
struct Source(u8);
impl NonceSource for Source {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        let value = self.0;
        if self.0 != 250 {
            self.0 += 1;
        }
        Ok([value; 16])
    }
}
fn config(mcs: u8) -> vradm_config_t {
    vradm_config_t {
        sample_rate: 8000,
        startup_mcs: mcs,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: 0.1334,
        reserved2: [0; 4],
        psk_key: [1; 16],
    }
}
fn step(
    ah: &mut EndpointHost<'_, 16, Source>,
    aa: &mut EndpointAudio<'_>,
    bh: &mut EndpointHost<'_, 16, Source>,
    ba: &mut EndpointAudio<'_>,
    now: u64,
    acknowledge: bool,
    drop_a: bool,
) {
    ah.pump(now).unwrap();
    bh.pump(now).unwrap();
    let mut a = [0; 160];
    let mut b = [0; 160];
    aa.generate_audio(&mut a);
    ba.generate_audio(&mut b);
    if drop_a {
        a.fill(0);
    }
    assert_eq!(aa.process_audio(&b), 0);
    assert_eq!(ba.process_audio(&a), 0);
    // Synchronous device harness: capture consumed the preceding output.
    if acknowledge {
        if let Some(fence) = aa.playback_fence() {
            assert!(aa.acknowledge_played(fence));
        }
        if let Some(fence) = ba.playback_fence() {
            assert!(ba.acknowledge_played(fence));
        }
    }
}
fn ready(
    ah: &mut EndpointHost<'_, 16, Source>,
    aa: &mut EndpointAudio<'_>,
    bh: &mut EndpointHost<'_, 16, Source>,
    ba: &mut EndpointAudio<'_>,
    now: &mut u64,
) {
    for _ in 0..1800 {
        step(ah, aa, bh, ba, *now, true, false);
        *now += 20;
        if ah.ready() && bh.ready() {
            return;
        }
    }
    panic!("endpoints did not become ready");
}
fn exchange(
    ah: &mut EndpointHost<'_, 16, Source>,
    aa: &mut EndpointAudio<'_>,
    bh: &mut EndpointHost<'_, 16, Source>,
    ba: &mut EndpointAudio<'_>,
    now: &mut u64,
    value: u8,
) {
    assert_eq!(ah.write_ip_packet(&[value; 19]), VRADM_OK);
    assert_eq!(bh.write_ip_packet(&[value + 1; 19]), VRADM_OK);
    let (mut got_a, mut got_b) = (false, false);
    for _ in 0..1200 {
        step(ah, aa, bh, ba, *now, true, false);
        *now += 20;
        let mut packet = [0; 296];
        let n = ah.poll_ip_packet(&mut packet);
        if n > 0 {
            assert!(!got_a);
            assert_eq!(n, 19);
            assert_eq!(&packet[..19], &[value + 1; 19]);
            got_a = true;
        }
        let n = bh.poll_ip_packet(&mut packet);
        if n > 0 {
            assert!(!got_b);
            assert_eq!(n, 19);
            assert_eq!(&packet[..19], &[value; 19]);
            got_b = true;
        }
        if got_a && got_b {
            return;
        }
    }
    panic!("bidirectional packets not delivered");
}

#[test]
fn public_endpoint_bootstraps_exchanges_and_rekeys_without_manual_transfers() {
    for mcs in [2, 3] {
        let mut a = Endpoint::<16>::new(config(mcs), 0).unwrap();
        let mut b = Endpoint::<16>::new(config(mcs), 0).unwrap();
        let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
        let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
        assert_eq!(ah.write_ip_packet(&[42; 19]), VRADM_ERR_STATE);
        ah.begin(0).unwrap();
        let mut now = 0;
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 42);
        // Cover an eight-frame data burst at MCS2 and a CCF burst at MCS3.
        if mcs == 2 {
            for _ in 0..250 {
                step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false);
                now += 20;
            }
        }
        let mut before: vradm_telemetry_t = unsafe { core::mem::zeroed() };
        ah.get_telemetry(&mut before);
        let compact_before = ah.ccf_counts().0;
        ah.write_ip_packet(&[99; 296]);
        aa.generate_audio(&mut [0; 17]);
        if mcs == 2 {
            let mut after: vradm_telemetry_t = unsafe { core::mem::zeroed() };
            ah.get_telemetry(&mut after);
            assert_eq!(after.frames_transmitted, before.frames_transmitted + 8);
        } else {
            assert_eq!(ah.ccf_counts().0, compact_before + 1);
        }
        ah.reset(now).unwrap();
        bh.reset(now).unwrap();
        assert!(!ah.ready() && !bh.ready());
        assert_eq!(ah.poll_ip_packet(&mut [0; 296]), VRADM_ERR_STATE);
        assert_eq!(ah.write_ip_packet(&[77; 19]), VRADM_ERR_STATE);
        // The other endpoint initiates the next epoch; roles are device-neutral.
        bh.begin(now).unwrap();
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 52);
    }
}

#[test]
fn device_drain_is_required_and_stale_fence_cannot_release_rekey() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    for _ in 0..500 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, false, false);
        now += 20;
        if aa.playback_fence().is_some() {
            break;
        }
    }
    let old = aa.playback_fence().expect("handshake drain fence");
    assert!(!ah.ready());
    assert_eq!(ah.write_ip_packet(&[42; 19]), VRADM_ERR_STATE);
    ah.reset(now).unwrap();
    bh.reset(now).unwrap();
    assert!(!aa.acknowledge_played(old));
    ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 62);
}

#[test]
fn automatic_bootstrap_retry_recovers_lost_request() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    while now < 2700 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, true);
        now += 20;
    }
    assert!(!ah.ready() && !bh.ready());
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 72);
}

#[test]
fn reset_backpressure_and_clock_regression_do_not_partially_reset_endpoint() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    ah.pump(now).unwrap();
    assert_eq!(ah.reset(now - 1), Err(EndpointError::ClockWentBackwards));
    assert!(ah.ready());
    for _ in 0..32 {
        assert_eq!(ah.set_tx_amplitude(0.1334), VRADM_OK);
    }
    assert_eq!(
        ah.reset(now),
        Err(EndpointError::Engine(VRADM_ERR_QUEUE_FULL))
    );
    assert!(ah.ready());
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 82);
}

#[test]
fn split_is_single_use_and_unsupported_profiles_are_rejected() {
    let mut endpoint = Endpoint::<16>::new(config(3), 0).unwrap();
    {
        let _handles = endpoint.split_with_entropy(Source(2)).unwrap();
    }
    assert!(matches!(endpoint.split(), Err(EndpointError::AlreadySplit)));
    for (rate, mcs, automatic) in [(16000, 3, 0), (8000, 0, 0), (8000, 3, 1)] {
        let mut cfg = config(mcs);
        cfg.sample_rate = rate;
        cfg.auto_rate_adaptation = automatic;
        assert!(matches!(
            Endpoint::<16>::new(cfg, 0),
            Err(EndpointError::Engine(VRADM_ERR_INVALID_ARG))
        ));
    }
}

#[test]
fn integrated_reset_preserves_bootstrap_nonce_history() {
    use vradm_core::{handshake::HandshakeError, handshake_bridge::WorkerError};
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(250)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    ah.reset(now).unwrap();
    assert_eq!(
        ah.begin(now),
        Err(EndpointError::Worker(WorkerError::Handshake(
            HandshakeError::Session(SessionError::NonceCollision)
        )))
    );
    assert!(!ah.ready());
}

#[test]
fn endpoint_recovers_lost_data_burst_without_duplicate_delivery() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    assert_eq!(ah.write_ip_packet(&[42; 19]), VRADM_OK);
    let mut count = 0;
    for index in 0..1200 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, index < 50);
        now += 20;
        let mut packet = [0; 296];
        let n = bh.poll_ip_packet(&mut packet);
        if n > 0 {
            assert_eq!(n, 19);
            assert_eq!(&packet[..19], &[42; 19]);
            count += 1;
        }
    }
    assert_eq!(count, 1);
}

#[test]
fn endpoint_uses_compact_ack_instead_of_canonical_feedback() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    ah.begin(0).unwrap();
    let mut now = 0;
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    assert_eq!(ah.write_ip_packet(&[42; 19]), VRADM_OK);
    for _ in 0..900 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false);
        now += 20;
    }
    assert_eq!(ah.ccf_counts(), (0, 1));
    assert_eq!(bh.ccf_counts(), (1, 0));
    let mut telemetry: vradm_telemetry_t = unsafe { core::mem::zeroed() };
    ah.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.frames_transmitted, 1);
    bh.get_telemetry(&mut telemetry);
    assert_eq!(telemetry.frames_transmitted, 0);
    assert_eq!(bh.poll_ip_packet(&mut [0; 296]), 19);
    assert_eq!(bh.poll_ip_packet(&mut [0; 296]), 0);
}

#[test]
fn lost_or_forged_compact_ack_recovers_with_fresh_transaction() {
    use vradm_core::{
        ccf_turn::CcfTurnTransmitter, framing::CompactControlFrame, security::SessionKeys,
    };
    for forged in [false, true] {
        let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
        let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
        let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
        let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
        ah.begin(0).unwrap();
        let mut now = 0;
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        let keys = SessionKeys::derive(&[1; 16], &[2; 16], &[20; 16]);
        let signed = keys
            .sign_ccf(
                1,
                CompactControlFrame {
                    ccf_ctrl: 0xb9,
                    ack_base: 0,
                    ack_map: 0,
                    ccf_mac: 0,
                },
            )
            .unwrap();
        let mut bad = CompactControlFrame::decode(signed, &[]).unwrap();
        bad.ccf_mac ^= 1;
        let mut replacement = CcfTurnTransmitter::new();
        replacement.start(bad.encode()).unwrap();
        ah.write_ip_packet(&[42; 19]);
        let mut delivered = 0;
        for _ in 0..1500 {
            ah.pump(now).unwrap();
            bh.pump(now).unwrap();
            let mut apcm = [0; 160];
            let mut bpcm = [0; 160];
            aa.generate_audio(&mut apcm);
            ba.generate_audio(&mut bpcm);
            if bh.ccf_counts().0 == 1 {
                if forged {
                    replacement.render(&mut bpcm);
                } else {
                    bpcm.fill(0);
                }
            }
            aa.process_audio(&bpcm);
            ba.process_audio(&apcm);
            let mut packet = [0; 296];
            if bh.poll_ip_packet(&mut packet) > 0 {
                assert_eq!(&packet[..19], &[42; 19]);
                delivered += 1;
            }
            now += 20;
        }
        assert_eq!(delivered, 1);
        assert_eq!(ah.ccf_counts(), (0, 1));
        assert_eq!(bh.ccf_counts(), (2, 0));
        let mut telemetry: vradm_telemetry_t = unsafe { core::mem::zeroed() };
        ah.get_telemetry(&mut telemetry);
        assert_eq!(telemetry.frames_transmitted, 2);
        assert_eq!(
            telemetry.security_tamper_detected,
            if forged { 1 } else { 0 }
        );
    }
}

#[test]
fn compact_feedback_engine_rejects_unnegotiated_rate_commands() {
    use vradm_core::engine::vradm_engine;
    let mut engine = vradm_engine::new_ccf_endpoint(config(3)).unwrap();
    let (mut host, _audio) = engine.split();
    let command = vradm_cmd_t {
        cmd_type: VRADM_CMD_REQUEST_MCS,
        cmd_id: 0,
        param_u32: 2,
        param_i32: 0,
        param_f32: 0.0,
        inline_payload: [0; 12],
    };
    assert_eq!(host.submit_cmd(&command), VRADM_ERR_STATE);
    assert_eq!(host.get_active_mcs(), 3);
}

fn telemetry(host: &EndpointHost<'_, 16, Source>) -> vradm_telemetry_t {
    let mut result = unsafe { core::mem::zeroed() };
    host.get_telemetry(&mut result);
    result
}

#[test]
fn live_upshift_waits_for_verified_reply_and_reliable_boundary() {
    use vradm_core::engine::RateChangeStatus as Rate;
    for lose_first in [false, true] {
        let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
        let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
        let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
        let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
        let mut now = 0;
        ah.begin(now).unwrap();
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        assert_eq!(bh.set_channel_metric(0.85), VRADM_OK);
        assert_eq!(ah.request_upshift(3), VRADM_OK);
        assert_eq!(ah.request_upshift(3), VRADM_ERR_STATE);
        let mut accepted = false;
        for _ in 0..1500 {
            ah.pump(now).unwrap(); bh.pump(now).unwrap();
            let (mut apcm, mut bpcm) = ([0; 160], [0; 160]);
            aa.generate_audio(&mut apcm); ba.generate_audio(&mut bpcm);
            if lose_first && bh.ccf_counts().0 == 1 { bpcm.fill(0); }
            aa.process_audio(&bpcm); ba.process_audio(&apcm);
            now += 20;
            assert_eq!(telemetry(&ah).active_tx_mcs, 2);
            assert_eq!(telemetry(&bh).active_rx_mcs, 2);
            if ah.rate_change_status() == Rate::AwaitingBoundary { accepted = true; break; }
        }
        assert!(accepted);
        assert_eq!(bh.ccf_counts().0, if lose_first { 2 } else { 1 });
        assert_eq!(ah.ccf_counts().1, 1);
        // BE_SEQ cannot satisfy the reliable commit boundary. It still flows
        // at the old rate and must not cancel the saved receive plan.
        let mut be = [0; 24]; be[0] = 0x45; be[9] = 17;
        be[22..24].copy_from_slice(&60000u16.to_be_bytes());
        assert_eq!(ah.write_ip_packet(&be), VRADM_OK);
        let mut got_be = false;
        for _ in 0..300 {
            step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
            let mut packet = [0; 296];
            if bh.poll_ip_packet(&mut packet) > 0 { assert_eq!(&packet[..24], &be); got_be = true; break; }
        }
        assert!(got_be);
        assert_eq!(telemetry(&ah).active_tx_mcs, 2);
        assert_eq!(ah.rate_change_status(), Rate::AwaitingBoundary);
        exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 41);
        assert_eq!(telemetry(&ah).active_tx_mcs, 3);
        assert_eq!(telemetry(&bh).active_rx_mcs, 3);
        assert_eq!(telemetry(&bh).active_tx_mcs, 2); // Independent directions.
        assert_eq!(ah.rate_change_status(), Rate::Idle);
        ah.reset(now).unwrap(); bh.reset(now).unwrap();
        ah.begin(now).unwrap();
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        assert_eq!(telemetry(&ah).active_tx_mcs, 2);
        assert_eq!(telemetry(&bh).active_rx_mcs, 2);
        exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 51);
    }
}

#[test]
fn live_upshift_refusal_aborts_three_attempts_then_releases_queued_data() {
    use vradm_core::engine::RateChangeStatus as Rate;
    let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    assert_eq!(bh.set_channel_metric(f32::NAN), VRADM_ERR_INVALID_ARG);
    assert_eq!(bh.set_channel_metric(0.84), VRADM_OK);
    assert_eq!(ah.request_upshift(4), VRADM_ERR_INVALID_ARG);
    assert_eq!(ah.request_upshift(3), VRADM_OK);
    assert_eq!(ah.write_ip_packet(&[67; 19]), VRADM_OK);
    let mut cooldown = false;
    for _ in 0..2100 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        assert_eq!(telemetry(&ah).active_tx_mcs, 2);
        if ah.rate_change_status() == Rate::Cooldown { cooldown = true; break; }
        assert_eq!(bh.poll_ip_packet(&mut [0; 296]), 0);
    }
    assert!(cooldown);
    assert_eq!(bh.ccf_counts().0, 3);
    assert_eq!(ah.ccf_counts().1, 0); // ACK response cannot masquerade as a commit.
    assert_eq!(ah.request_upshift(3), VRADM_ERR_STATE);
    assert_eq!(ah.emergency_downshift(0.5), VRADM_OK);
    let mut delivered = 0;
    for tick in 0..550 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        if tick < 450 { assert_eq!(ah.rate_change_status(), Rate::Cooldown); }
        let mut packet = [0; 296];
        if bh.poll_ip_packet(&mut packet) > 0 { assert_eq!(&packet[..19], &[67; 19]); delivered += 1; }
    }
    assert_eq!(delivered, 1);
    assert_eq!(ah.rate_change_status(), Rate::Idle);
    assert_eq!(bh.set_channel_metric(0.95), VRADM_OK);
    assert_eq!(ah.request_upshift(3), VRADM_OK);
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 77);
    assert_eq!(telemetry(&ah).active_tx_mcs, 3);
}

#[test]
fn live_upshift_drains_old_window_and_commits_across_sequence_wrap() {
    use vradm_core::engine::RateChangeStatus as Rate;
    let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    bh.set_channel_metric(0.95);
    // Advance to REL_SEQ 255 using actual endpoint traffic, never injected ARQ.
    for packet_index in 0..32 {
        let len = if packet_index == 31 { 259 } else { 296 };
        assert_eq!(ah.write_ip_packet(&[71; 296][..len]), VRADM_OK);
        let mut delivered = 0;
        for _ in 0..600 {
            step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
            let mut packet = [0; 296];
            let n = bh.poll_ip_packet(&mut packet);
            if n > 0 { assert_eq!(n as usize, len); delivered += 1; }
            if ah.ccf_counts().1 == packet_index + 1 { break; }
        }
        assert_eq!(delivered, 1);
        assert_eq!(ah.ccf_counts().1, packet_index + 1);
    }
    assert_eq!(ah.write_ip_packet(&[72; 19]), VRADM_OK);
    step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
    assert_eq!(ah.request_upshift(3), VRADM_OK); // Seq 255 already in flight.
    assert_eq!(ah.write_ip_packet(&[73; 19]), VRADM_OK); // Retained for seq 0.
    let mut packets = Vec::new();
    for _ in 0..1200 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        if ah.ccf_counts().1 < 33 {
            assert_eq!(ah.rate_change_status(), Rate::Draining);
            assert_eq!(telemetry(&ah).active_tx_mcs, 2);
        }
        let mut packet = [0; 296];
        if bh.poll_ip_packet(&mut packet) > 0 { packets.push(packet[0]); }
        if ah.ccf_counts().1 == 35 { break; } // 33 data ACKs + commit + final ACK.
    }
    assert_eq!(packets, vec![72, 73]);
    assert_eq!(telemetry(&ah).active_tx_mcs, 3);
    assert_eq!(telemetry(&bh).active_rx_mcs, 3);
    assert_eq!(ah.ccf_counts().1, 35);
}

#[test]
fn live_upshift_forged_commit_does_not_release_old_rate() {
    use vradm_core::{engine::RateChangeStatus as Rate, ccf_turn::CcfTurnTransmitter,
        framing::CompactControlFrame, security::SessionKeys};
    let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    bh.set_channel_metric(0.95); ah.request_upshift(3);
    let keys = SessionKeys::derive(&[1;16], &[2;16], &[20;16]);
    let wire = keys.sign_ccf(1, CompactControlFrame {
        ccf_ctrl: 0xba, ack_base: 255, ack_map: 0, ccf_mac: 0,
    }).unwrap();
    let mut bad = CompactControlFrame::decode(wire, &[]).unwrap(); bad.ccf_mac ^= 1;
    let mut replacement = CcfTurnTransmitter::new(); replacement.start(bad.encode()).unwrap();
    for _ in 0..1400 {
        ah.pump(now).unwrap(); bh.pump(now).unwrap();
        let (mut apcm, mut bpcm) = ([0;160], [0;160]);
        aa.generate_audio(&mut apcm); ba.generate_audio(&mut bpcm);
        if bh.ccf_counts().0 == 1 { replacement.render(&mut bpcm); }
        aa.process_audio(&bpcm); ba.process_audio(&apcm); now += 20;
        assert_eq!(telemetry(&ah).active_tx_mcs, 2);
        if ah.rate_change_status() == Rate::AwaitingBoundary { break; }
    }
    assert_eq!(ah.rate_change_status(), Rate::AwaitingBoundary);
    assert_eq!(bh.ccf_counts().0, 2);
    assert_eq!(telemetry(&ah).security_tamper_detected, 1);
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 91);
    assert_eq!(telemetry(&ah).active_tx_mcs, 3);
}

#[test]
fn endpoint_rejects_authenticated_target_rate_without_receive_commit() {
    use vradm_core::{arq::IpPacketSlicer, phy::PhyTransmitter,
        security::{ControlTx, SessionKeys}};
    let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    let mut control = ControlTx::new(SessionKeys::derive(&[1;16], &[2;16], &[20;16]));
    control.beacon(2, 2, 0).unwrap(); // Counter zero was the bootstrap confirmation.
    let beacon = control.beacon(3, 3, 0).unwrap();
    let mut frames = IpPacketSlicer::slice(&[42;19], false, false, 0).unwrap();
    frames[0].ctrl |= 0x38;
    let mut tx = PhyTransmitter::new(3);
    for chunk in tx.modulate_authenticated(beacon, &frames, true).unwrap().chunks(160) {
        ba.process_audio(chunk);
    }
    assert_eq!(bh.poll_ip_packet(&mut [0;296]), 0);
    assert_eq!(telemetry(&bh).active_rx_mcs, 2);
    assert_eq!(telemetry(&bh).security_tamper_detected, 0); // Valid MAC, refused policy.
    assert_eq!(bh.ccf_counts().0, 0);
}

#[test]
fn reset_clears_pending_commit_and_metric_and_command_pressure_preserves_admission() {
    use vradm_core::engine::RateChangeStatus as Rate;
    let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    for _ in 0..32 { assert_eq!(ah.set_tx_amplitude(0.1334), VRADM_OK); }
    assert_eq!(ah.request_upshift(3), VRADM_ERR_QUEUE_FULL);
    assert_eq!(ah.rate_change_status(), Rate::Idle);
    step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
    assert_eq!(ah.request_upshift(3), VRADM_OK);
    assert_eq!(bh.set_channel_metric(0.95), VRADM_OK);
    for _ in 0..400 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        if ah.rate_change_status() == Rate::AwaitingBoundary { break; }
    }
    assert_eq!(ah.rate_change_status(), Rate::AwaitingBoundary);
    ah.reset(now).unwrap(); bh.reset(now).unwrap(); ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    assert_eq!(ah.rate_change_status(), Rate::Idle);
    assert_eq!(ah.request_upshift(3), VRADM_OK);
    for _ in 0..2000 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        if ah.rate_change_status() == Rate::Cooldown { break; }
    }
    assert_eq!(ah.rate_change_status(), Rate::Cooldown); // No metric after reset.
    assert_eq!(telemetry(&ah).active_tx_mcs, 2);
    assert_eq!(telemetry(&bh).active_rx_mcs, 2);
    assert_eq!(bh.ccf_counts().0, 3);
}

#[test]
fn emergency_downshift_announces_when_idle_and_rejects_invalid_metrics() {
    let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
    let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
    let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
    let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
    assert_eq!(ah.emergency_downshift(0.5), VRADM_ERR_STATE);
    let mut now = 0; ah.begin(now).unwrap();
    ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
    for metric in [f32::NAN, f32::INFINITY, -0.01, 0.60, 1.0] {
        assert_eq!(ah.emergency_downshift(metric), VRADM_ERR_INVALID_ARG);
    }
    for _ in 0..32 { assert_eq!(ah.set_tx_amplitude(0.1334), VRADM_OK); }
    assert_eq!(ah.emergency_downshift(0.59), VRADM_ERR_QUEUE_FULL);
    assert_eq!(telemetry(&ah).active_tx_mcs, 3);
    step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
    assert_eq!(ah.emergency_downshift(0.59), VRADM_OK);
    for _ in 0..100 {
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
    }
    assert_eq!(telemetry(&ah).active_tx_mcs, 2);
    assert_eq!(telemetry(&bh).active_rx_mcs, 2);
    assert_eq!(telemetry(&bh).active_tx_mcs, 3);
    assert_eq!(ah.ccf_counts(), (0, 0)); // No commit or data ACK was required.
    assert_eq!(telemetry(&ah).frames_transmitted, 1);
    exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 81);
}

#[test]
fn emergency_downshift_finishes_buffered_audio_then_retries_unacked_data_at_mcs2() {
    for lose_data in [true, false] {
        let mut a = Endpoint::<16>::new(config(3), 0).unwrap();
        let mut b = Endpoint::<16>::new(config(3), 0).unwrap();
        let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
        let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
        let mut now = 0; ah.begin(now).unwrap();
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        assert_eq!(ah.write_ip_packet(&[85; 19]), VRADM_OK);
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, lose_data); now += 20;
        assert_eq!(ah.emergency_downshift(0.4), VRADM_OK);
        let mut delivered = 0;
        let mut changed = false;
        for _ in 0..500 {
            ah.pump(now).unwrap(); bh.pump(now).unwrap();
            let (mut apcm, mut bpcm) = ([0;160], [0;160]);
            aa.generate_audio(&mut apcm); ba.generate_audio(&mut bpcm);
            if telemetry(&ah).active_tx_mcs == 3 {
                assert!(!changed);
                if lose_data { apcm.fill(0); }
            } else { changed = true; }
            if !lose_data && bh.ccf_counts().0 == 1 { bpcm.fill(0); }
            aa.process_audio(&bpcm); ba.process_audio(&apcm); now += 20;
            let mut packet = [0;296];
            if bh.poll_ip_packet(&mut packet) > 0 {
                assert_eq!(&packet[..19], &[85;19]); delivered += 1;
            }
        }
        assert!(changed);
        assert_eq!(delivered, 1);
        assert_eq!(telemetry(&ah).frames_transmitted, 2);
        assert_eq!(telemetry(&bh).active_rx_mcs, 2);
        assert_eq!(ah.ccf_counts().1, 1);
    }
}

#[test]
fn emergency_downshift_cancels_both_request_and_accepted_commit_without_losing_packets() {
    use vradm_core::engine::RateChangeStatus as Rate;
    for cancel_at in [Rate::AwaitingReply, Rate::AwaitingBoundary] {
        let mut a = Endpoint::<16>::new(config(2), 0).unwrap();
        let mut b = Endpoint::<16>::new(config(2), 0).unwrap();
        let (mut ah, mut aa) = a.split_with_entropy(Source(2)).unwrap();
        let (mut bh, mut ba) = b.split_with_entropy(Source(20)).unwrap();
        let mut now = 0; ah.begin(now).unwrap();
        ready(&mut ah, &mut aa, &mut bh, &mut ba, &mut now);
        assert_eq!(ah.emergency_downshift(0.5), VRADM_OK);
        assert_eq!(ah.request_upshift(3), VRADM_ERR_STATE); // Idle MCS2, emergency queued.
        step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
        bh.set_channel_metric(0.95);
        assert_eq!(ah.request_upshift(3), VRADM_OK);
        for _ in 0..400 {
            step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
            if ah.rate_change_status() == cancel_at { break; }
        }
        assert_eq!(ah.rate_change_status(), cancel_at);
        assert_eq!(ah.emergency_downshift(0.5), VRADM_OK);
        assert_eq!(ah.emergency_downshift(0.5), VRADM_ERR_STATE);
        assert_eq!(ah.request_upshift(3), VRADM_ERR_STATE);
        assert_eq!(ah.write_ip_packet(&[87;19]), VRADM_OK);
        let mut delivered = 0;
        for _ in 0..600 {
            step(&mut ah, &mut aa, &mut bh, &mut ba, now, true, false); now += 20;
            assert_eq!(telemetry(&ah).active_tx_mcs, 2);
            let mut packet = [0;296];
            if bh.poll_ip_packet(&mut packet) > 0 {
                assert_eq!(&packet[..19], &[87;19]); delivered += 1;
            }
        }
        assert_eq!(delivered, 1);
        assert_eq!(ah.rate_change_status(), Rate::Idle);
        assert_eq!(telemetry(&bh).active_rx_mcs, 2);
        assert_eq!(ah.request_upshift(3), VRADM_OK);
        exchange(&mut ah, &mut aa, &mut bh, &mut ba, &mut now, 88);
        assert_eq!(telemetry(&ah).active_tx_mcs, 3);
    }
}
