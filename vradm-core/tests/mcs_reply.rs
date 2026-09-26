use vradm_core::{ccf_turn::*, mcs_control::McsNegotiator, security::*};
fn keys() -> SessionKeys {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
}
fn local(metric: f32) -> McsReplyParameters {
    McsReplyParameters {
        channel_metric: metric,
        ack_base: 255,
        ack_map: 3,
        yield_turn: true,
    }
}

#[test]
fn verified_request_produces_audio_reply_and_deferred_sender_commit() {
    let mut sender = ControlTx::new(keys());
    let mut sender_rx = ControlRx::new(keys(), 0);
    let mut receiver = ControlRx::new(keys(), 0);
    let mut policy = McsNegotiator::new(&mut sender, &mut sender_rx, 2, 0).unwrap();
    let request = policy.request_upshift(3, 0, true, 5000, 0).unwrap();
    let reply = receiver
        .prepare_mcs_commit(request, local(0.85), 1)
        .unwrap();
    assert_eq!(reply.request_counter(), 0);
    assert_eq!(reply.target_mcs(), 3);
    assert_eq!(reply.first_sequence(), 0);
    let mut tx = CcfTurnTransmitter::new();
    tx.start(reply.codeword()).unwrap();
    let mut pcm = vec![0; CCF_TURN_SAMPLES];
    tx.render(&mut pcm);
    let decoded = CcfTurnReceiver::at_frame_start()
        .push(&pcm)
        .frame
        .unwrap()
        .unwrap();
    let verified = policy
        .receive(decoded.codeword(), decoded.erasures(), 1901)
        .unwrap();
    assert_eq!(verified.commit_sequence, Some(0));
    assert_eq!(policy.current_mcs(), 2);
    policy.complete_commit(1901).unwrap();
    assert_eq!(policy.current_mcs(), 3);
}

#[test]
fn forged_and_replayed_requests_cannot_produce_responses() {
    let mut tx = ControlTx::new(keys());
    let request = tx.beacon(2, 3, 0).unwrap();
    let mut bad = request;
    bad.mac ^= 1;
    let mut rx = ControlRx::new(keys(), 0);
    assert_eq!(
        rx.prepare_mcs_commit(bad, local(1.0), 0).unwrap_err(),
        McsReplyError::Security(SecurityError::BadMac)
    );
    assert_eq!(rx.mac_failures(), 1);
    rx.prepare_mcs_commit(request, local(1.0), 0).unwrap();
    assert_eq!(
        rx.prepare_mcs_commit(request, local(1.0), 0).unwrap_err(),
        McsReplyError::Security(SecurityError::Replay)
    );
}

#[test]
fn metric_and_request_direction_gate_reply_without_reusing_refused_counter() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let request = tx.beacon(2, 3, 0).unwrap();
    assert_eq!(
        rx.prepare_mcs_commit(request, local(0.8499), 0)
            .unwrap_err(),
        McsReplyError::InsufficientMetric
    );
    assert_eq!(
        rx.prepare_mcs_commit(request, local(0.9), 0).unwrap_err(),
        McsReplyError::Security(SecurityError::Replay)
    );
    for target in [2, 1] {
        assert_eq!(
            rx.prepare_mcs_commit(tx.beacon(2, target, 0).unwrap(), local(1.0), 0)
                .unwrap_err(),
            McsReplyError::NotUpshift
        );
    }
    assert!(rx
        .prepare_mcs_commit(tx.beacon(2, 3, 0).unwrap(), local(0.85), 0)
        .is_ok());
}

#[test]
fn invalid_local_parameters_do_not_consume_peer_request() {
    let request = ControlTx::new(keys()).beacon(2, 3, 0).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    for metric in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        assert_eq!(
            rx.prepare_mcs_commit(request, local(metric), 0)
                .unwrap_err(),
            McsReplyError::InvalidLocalParameters
        );
    }
    let mut params = local(1.0);
    params.ack_map = 128;
    assert_eq!(
        rx.prepare_mcs_commit(request, params, 0).unwrap_err(),
        McsReplyError::InvalidLocalParameters
    );
    assert!(rx.prepare_mcs_commit(request, local(1.0), 0).is_ok());
}

#[test]
fn delayed_verified_request_cannot_override_newer_peer_announcement() {
    let mut tx = ControlTx::new(keys());
    let older = tx.beacon(2, 3, 0).unwrap();
    let newer = tx.beacon(2, 2, 0).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    rx.verify_beacon(newer, 0).unwrap();
    assert_eq!(
        rx.prepare_mcs_commit(older, local(1.0), 0).unwrap_err(),
        McsReplyError::StaleRequest
    );
    assert_eq!(rx.mac_failures(), 0);
}

#[test]
fn full_counter_wrap_binding_and_yield_semantics_are_preserved() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut verifier = ControlRx::new(keys(), 0);
    for counter in 0..=256u16 {
        let beacon = tx.beacon(2, 3, 0).unwrap();
        let mut params = local(0.9);
        params.yield_turn = false;
        let reply = rx.prepare_mcs_commit(beacon, params, 0).unwrap();
        assert_eq!(reply.request_counter(), counter);
        let decoded = verifier
            .verify_ccf(reply.codeword(), &[], counter, 0)
            .unwrap();
        assert_eq!(decoded.ccf_ctrl, 0xb2);
    }
}

#[test]
fn invalidated_session_and_shared_mac_budget_block_response_creation() {
    let request = ControlTx::new(keys()).beacon(2, 3, 0).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    let mut forged = request;
    forged.mac ^= 1;
    for _ in 0..10 {
        assert_eq!(
            rx.prepare_mcs_commit(forged, local(1.0), 0).unwrap_err(),
            McsReplyError::Security(SecurityError::BadMac)
        );
    }
    assert_eq!(
        rx.prepare_mcs_commit(request, local(1.0), 0).unwrap_err(),
        McsReplyError::Security(SecurityError::RateLimited)
    );
    rx.invalidate();
    assert_eq!(
        rx.prepare_mcs_commit(request, local(1.0), 1000)
            .unwrap_err(),
        McsReplyError::Security(SecurityError::ResyncRequired)
    );
}
