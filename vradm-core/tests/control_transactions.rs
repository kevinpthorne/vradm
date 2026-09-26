use vradm_core::{framing::CompactControlFrame, security::*};

fn keys() -> SessionKeys {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
}
fn request(command: ControlCommand, deadline_ms: u64) -> ControlRequest {
    ControlRequest {
        current_mcs: 2,
        target_mcs: 3,
        tx_power: 0,
        command,
        yield_turn: true,
        deadline_ms,
    }
}
fn reply(counter: u16, ctrl: u8, base: u8, map: u8) -> [u8; 16] {
    keys()
        .sign_ccf(
            counter,
            CompactControlFrame {
                ccf_ctrl: ctrl,
                ack_base: base,
                ack_map: map,
                ccf_mac: 0,
            },
        )
        .unwrap()
}
fn forged(wire: [u8; 16]) -> [u8; 16] {
    let mut frame = CompactControlFrame::decode(wire, &[]).unwrap();
    frame.ccf_mac ^= 1;
    frame.encode()
}

#[test]
fn only_one_request_can_be_outstanding_and_no_beacon_can_change_its_counter() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    assert_eq!(
        tx.begin_control(request(ControlCommand::TddGrant, 1000), 0)
            .unwrap()
            .sequence,
        0
    );
    assert_eq!(tx.beacon(2, 2, 0), Err(SecurityError::OutstandingRequest));
    assert_eq!(
        tx.begin_control(request(ControlCommand::McsCommitAck, 1000), 1),
        Err(SecurityError::OutstandingRequest)
    );
    assert_eq!(tx.last_counter(), Some(0));
    let response = rx
        .verify_control_response(&mut tx, reply(0, 0xbb, 7, 3), &[], 2)
        .unwrap();
    assert!(response.apply_semantics);
    assert_eq!(response.commit_sequence, None);
    assert_eq!(
        tx.begin_control(request(ControlCommand::StandaloneAck, 1000), 3)
            .unwrap()
            .sequence,
        1
    );
}

#[test]
fn duplicate_commit_refreshes_acks_without_changing_the_first_wrap_boundary() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::McsCommitAck, 1000), 0)
        .unwrap();
    let first = rx
        .verify_control_response(&mut tx, reply(0, 0xba, 255, 1), &[], 1)
        .unwrap();
    assert!(first.apply_semantics);
    assert_eq!(first.commit_sequence, Some(0));
    let duplicate = rx
        .verify_control_response(&mut tx, reply(0, 0xba, 4, 7), &[], 2)
        .unwrap();
    assert!(!duplicate.apply_semantics);
    assert_eq!(duplicate.commit_sequence, Some(0));
    assert_eq!(duplicate.frame.ack_base, 4);
    assert_eq!(duplicate.frame.ack_map, 7);
    tx.beacon(3, 3, 0).unwrap();
    assert_eq!(
        rx.verify_control_response(&mut tx, reply(0, 0xba, 4, 7), &[], 3)
            .unwrap_err(),
        SecurityError::NoPendingRequest
    );
}

#[test]
fn wrong_type_target_or_yield_cannot_consume_the_matching_response() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::McsCommitAck, 1000), 0)
        .unwrap();
    for ctrl in [0xbb, 0xaa, 0xb2] {
        assert_eq!(
            rx.verify_control_response(&mut tx, reply(0, ctrl, 7, 3), &[], 0)
                .unwrap_err(),
            SecurityError::UnexpectedControl
        );
    }
    for (ctrl, map) in [(0x3a, 0), (0xfa, 0), (0xb8, 0), (0xba, 128)] {
        assert_eq!(
            rx.verify_control_response(&mut tx, reply(0, ctrl, 7, map), &[], 0)
                .unwrap_err(),
            SecurityError::Malformed
        );
    }
    assert_eq!(rx.mac_failures(), 0);
    assert!(
        rx.verify_control_response(&mut tx, reply(0, 0xba, 7, 3), &[], 0)
            .unwrap()
            .apply_semantics
    );
    assert_eq!(
        rx.verify_control_response(&mut tx, reply(0, 0xbb, 7, 3), &[], 0)
            .unwrap_err(),
        SecurityError::UnexpectedControl
    );
    assert!(
        !rx.verify_control_response(&mut tx, reply(0, 0xba, 7, 3), &[], 0)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn timeout_local_rejection_and_clock_errors_never_reuse_a_counter() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::StandaloneAck, 100), 10)
        .unwrap();
    assert_eq!(tx.expire_control(9), Err(SecurityError::ClockWentBackwards));
    assert_eq!(tx.expire_control(99), Ok(false));
    assert_eq!(
        rx.verify_control_response(&mut tx, reply(0, 0xb9, 0, 0), &[], 100)
            .unwrap_err(),
        SecurityError::NoPendingRequest
    );
    assert_eq!(tx.expire_control(100), Ok(false));
    assert_eq!(
        tx.begin_control(request(ControlCommand::StandaloneAck, 100), 100),
        Err(SecurityError::Malformed)
    );
    assert_eq!(tx.last_counter(), Some(0));
    assert_eq!(
        tx.begin_control(request(ControlCommand::StandaloneAck, 200), 100)
            .unwrap()
            .sequence,
        1
    );
    tx.reject_control(101).unwrap();
    assert_eq!(
        rx.verify_control_response(&mut tx, reply(1, 0xb9, 0, 0), &[], 102)
            .unwrap_err(),
        SecurityError::NoPendingRequest
    );
    assert_eq!(
        tx.begin_control(request(ControlCommand::StandaloneAck, 200), 102)
            .unwrap()
            .sequence,
        2
    );
}

#[test]
fn stale_reply_at_wire_counter_wrap_is_not_a_response_to_the_new_request() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::StandaloneAck, 1000), 0)
        .unwrap();
    // MAC8 can collide: use a fixture whose tags differ at these two counters.
    let base = (0..=255)
        .find(|&base| reply(0, 0xb9, base, 0)[7] != reply(256, 0xb9, base, 0)[7])
        .unwrap();
    let old = reply(0, 0xb9, base, 0);
    rx.verify_control_response(&mut tx, old, &[], 0).unwrap();
    for _ in 1..256 {
        tx.beacon(2, 2, 0).unwrap();
    }
    assert_eq!(
        tx.begin_control(request(ControlCommand::StandaloneAck, 1000), 1)
            .unwrap()
            .sequence,
        0
    );
    assert_eq!(tx.last_counter(), Some(256));
    assert_eq!(
        rx.verify_control_response(&mut tx, old, &[], 1)
            .unwrap_err(),
        SecurityError::BadMac
    );
    assert!(
        rx.verify_control_response(&mut tx, reply(256, 0xb9, base, 0), &[], 1)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn ccf_transactions_share_plcp_failure_budget_and_keep_channel_errors_separate() {
    let mut tx = ControlTx::new(keys());
    let mut peer = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::StandaloneAck, 1000), 0)
        .unwrap();
    let good = reply(0, 0xb9, 0, 0);
    let mut info: [u8; 8] = good[..8].try_into().unwrap();
    info[5] ^= 1; // Re-encode RS around deliberately invalid CRC.
    let noise = vradm_core::fec::rs_encode_16_8(&info);
    for _ in 0..20 {
        assert_eq!(
            rx.verify_control_response(&mut tx, noise, &[], 0)
                .unwrap_err(),
            SecurityError::ChannelIntegrity
        );
    }
    assert_eq!(rx.mac_failures(), 0);
    let mut beacon = peer.beacon(2, 2, 0).unwrap();
    beacon.mac ^= 1;
    for _ in 0..5 {
        assert_eq!(rx.verify_beacon(beacon, 0), Err(SecurityError::BadMac));
    }
    for _ in 0..5 {
        assert_eq!(
            rx.verify_control_response(&mut tx, forged(good), &[], 0)
                .unwrap_err(),
            SecurityError::BadMac
        );
    }
    assert_eq!(
        rx.verify_control_response(&mut tx, good, &[], 99)
            .unwrap_err(),
        SecurityError::RateLimited
    );
    assert_eq!(rx.mac_failures(), 10);
    assert!(
        rx.verify_control_response(&mut tx, good, &[], 100)
            .unwrap()
            .apply_semantics
    );
    assert_eq!(
        rx.verify_control_response(&mut tx, forged(good), &[], 100)
            .unwrap_err(),
        SecurityError::BadMac
    );
    assert!(
        !rx.verify_control_response(&mut tx, good, &[], 200)
            .unwrap()
            .apply_semantics
    );
}

#[test]
fn verified_duplicate_context_expires_and_outage_blocks_responses() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(request(ControlCommand::StandaloneAck, 100), 0)
        .unwrap();
    let wire = reply(0, 0xb9, 0, 0);
    rx.verify_control_response(&mut tx, wire, &[], 0).unwrap();
    assert!(
        !rx.verify_control_response(&mut tx, wire, &[], 99)
            .unwrap()
            .apply_semantics
    );
    assert_eq!(
        rx.verify_control_response(&mut tx, wire, &[], 100)
            .unwrap_err(),
        SecurityError::NoPendingRequest
    );
    tx.begin_control(request(ControlCommand::StandaloneAck, 200), 100)
        .unwrap();
    rx.invalidate();
    assert_eq!(
        rx.verify_control_response(&mut tx, reply(1, 0xb9, 0, 0), &[], 101)
            .unwrap_err(),
        SecurityError::ResyncRequired
    );
}
