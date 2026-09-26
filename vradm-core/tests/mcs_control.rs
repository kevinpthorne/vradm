use vradm_core::{framing::CompactControlFrame, mcs_control::*, security::*};
fn keys() -> SessionKeys {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
}
fn ack(counter: u16, base: u8) -> [u8; 16] {
    keys()
        .sign_ccf(
            counter,
            CompactControlFrame {
                ccf_ctrl: 0xba,
                ack_base: base,
                ack_map: 3,
                ccf_mac: 0,
            },
        )
        .unwrap()
}
fn emitted(event: McsControlEvent) -> Beacon {
    match event {
        McsControlEvent::Transmit(beacon) => beacon,
        other => panic!("expected retry, got {other:?}"),
    }
}

#[test]
fn three_attempts_then_cooldown_keep_current_rate_and_data_available() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    let first = m.request_upshift(3, 0, true, 100, 0).unwrap();
    assert_eq!(
        (first.current_mcs, first.requested_mcs, first.sequence),
        (2, 3, 0)
    );
    assert_eq!(m.data_beacon(0, 0), Err(McsControlError::Busy));
    assert_eq!(m.poll(99), Ok(McsControlEvent::None));
    assert_eq!(emitted(m.poll(100).unwrap()).sequence, 1);
    assert_eq!(emitted(m.poll(200).unwrap()).sequence, 2);
    assert_eq!(m.current_mcs(), 2);
    assert_eq!(m.poll(300), Ok(McsControlEvent::Aborted));
    assert_eq!(m.requested_mcs(), 2);
    assert_eq!(m.cooldown_remaining_ms(300), 10000);
    assert_eq!(
        m.request_upshift(3, 0, true, 100, 10299),
        Err(McsControlError::CoolingDown)
    );
    let data = m.data_beacon(0, 10299).unwrap();
    assert_eq!(
        (data.current_mcs, data.requested_mcs, data.sequence),
        (2, 2, 3)
    );
    assert_eq!(
        m.request_upshift(3, 0, true, 100, 10300).unwrap().sequence,
        4
    );
}

#[test]
fn verified_commit_waits_for_local_sequence_boundary_application() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    m.request_upshift(3, 0, true, 100, 0).unwrap();
    assert!(m.receive(ack(0, 255), &[], 1).unwrap().apply_semantics);
    let plan = McsCommit {
        previous_mcs: 2,
        target_mcs: 3,
        first_sequence: 0,
    };
    assert_eq!(m.pending_commit(), Some(plan));
    assert_eq!(m.current_mcs(), 2);
    assert_eq!(m.data_beacon(0, 1), Err(McsControlError::Busy));
    assert!(!m.receive(ack(0, 7), &[], 2).unwrap().apply_semantics);
    assert_eq!(m.pending_commit(), Some(plan));
    assert_eq!(m.poll(1000), Ok(McsControlEvent::None)); // Accepted plan survives reply-context expiry.
    assert_eq!(m.complete_commit(1000), Ok(plan));
    assert_eq!(m.data_beacon(0, 1000).unwrap().current_mcs, 3);
    assert_eq!(m.complete_commit(1000), Err(McsControlError::NoCommit));
}

#[test]
fn forged_or_late_reply_never_upshifts_and_retry_uses_fresh_counter() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    m.request_upshift(3, 0, true, 100, 0).unwrap();
    let mut bad = CompactControlFrame::decode(ack(0, 7), &[]).unwrap();
    bad.ccf_mac ^= 1;
    assert_eq!(
        m.receive(bad.encode(), &[], 99).unwrap_err(),
        McsControlError::Security(SecurityError::BadMac)
    );
    assert_eq!(m.pending_commit(), None);
    assert_eq!(
        m.receive(ack(0, 7), &[], 100).unwrap_err(),
        McsControlError::Security(SecurityError::NoPendingRequest)
    );
    assert_eq!(emitted(m.poll(100).unwrap()).sequence, 1);
    assert_eq!(m.current_mcs(), 2);
    assert!(m.receive(ack(1, 7), &[], 101).unwrap().apply_semantics);
    assert_eq!(m.pending_commit().unwrap().first_sequence, 8);
}

#[test]
fn late_poll_emits_only_one_retry_and_cooldown_cannot_overflow_open() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    m.request_upshift(3, 0, true, 100, 0).unwrap();
    assert_eq!(emitted(m.poll(100000).unwrap()).sequence, 1);
    assert_eq!(m.poll(100000), Ok(McsControlEvent::None));
    assert_eq!(emitted(m.poll(100100).unwrap()).sequence, 2);
    assert_eq!(m.poll(u64::MAX), Ok(McsControlEvent::Aborted));
    assert_eq!(m.cooldown_remaining_ms(u64::MAX), 10000);
    assert_eq!(
        m.request_upshift(3, 0, true, 100, u64::MAX),
        Err(McsControlError::CoolingDown)
    );
}

#[test]
fn emergency_downshift_cancels_pending_or_accepted_upshift_without_ack() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    m.request_upshift(3, 0, true, 100, 0).unwrap();
    m.emergency_downshift(1, 1).unwrap();
    assert_eq!(m.data_beacon(0, 1).unwrap().current_mcs, 1);
    assert_eq!(
        m.receive(ack(0, 7), &[], 1).unwrap_err(),
        McsControlError::NoActiveRequest
    );
    let request = m.request_upshift(3, 0, true, 100, 2).unwrap();
    m.receive(ack(request.sequence as u16, 7), &[], 3).unwrap();
    m.emergency_downshift(0, 4).unwrap();
    assert_eq!(m.pending_commit(), None);
    assert_eq!(m.current_mcs(), 0);
    assert_eq!(m.complete_commit(4), Err(McsControlError::NoCommit));
}

#[test]
fn invalid_requests_and_clock_regression_do_not_consume_counters() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 10);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 10).unwrap();
    assert_eq!(
        m.request_upshift(2, 0, true, 100, 10),
        Err(McsControlError::InvalidMcs)
    );
    assert_eq!(
        m.request_upshift(5, 0, true, 100, 10),
        Err(McsControlError::InvalidMcs)
    );
    assert_eq!(
        m.request_upshift(3, 0, true, 0, 10),
        Err(McsControlError::InvalidTiming)
    );
    assert_eq!(
        m.request_upshift(3, 0, true, u64::MAX, 10),
        Err(McsControlError::InvalidTiming)
    );
    assert_eq!(
        m.request_upshift(3, 4, true, 100, 10),
        Err(McsControlError::Security(SecurityError::Malformed))
    );
    assert_eq!(
        m.poll(9),
        Err(McsControlError::Security(SecurityError::ClockWentBackwards))
    );
    assert_eq!(m.request_upshift(3, 0, true, 100, 10).unwrap().sequence, 0);
    assert_eq!(
        m.request_upshift(4, 0, true, 100, 10),
        Err(McsControlError::Busy)
    );
}

#[test]
fn downshift_and_cancel_do_not_erase_failed_upshift_cooldown() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    m.request_upshift(3, 0, true, 100, 0).unwrap();
    m.poll(100).unwrap();
    m.poll(200).unwrap();
    m.poll(300).unwrap();
    m.emergency_downshift(1, 400).unwrap();
    m.cancel(500).unwrap();
    assert_eq!(m.cooldown_remaining_ms(500), 9800);
    assert_eq!(
        m.request_upshift(2, 0, true, 100, 500),
        Err(McsControlError::CoolingDown)
    );
    assert_eq!(m.data_beacon(0, 500).unwrap().current_mcs, 1);
}

#[test]
fn existing_request_is_not_taken_over_and_rekey_exhaustion_does_not_arm_retry() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    tx.begin_control(
        ControlRequest {
            current_mcs: 2,
            target_mcs: 2,
            tx_power: 0,
            command: ControlCommand::StandaloneAck,
            yield_turn: false,
            deadline_ms: 100,
        },
        0,
    )
    .unwrap();
    assert!(matches!(
        McsNegotiator::new(&mut tx, &mut rx, 2, 0),
        Err(McsControlError::Busy)
    ));
    tx.reject_control(0).unwrap();
    for _ in 1..REKEY_COUNTER {
        tx.beacon(2, 2, 0).unwrap();
    }
    let mut m = McsNegotiator::new(&mut tx, &mut rx, 2, 0).unwrap();
    assert_eq!(
        m.request_upshift(3, 0, true, 100, 0),
        Err(McsControlError::Security(SecurityError::RekeyRequired))
    );
    assert_eq!(m.requested_mcs(), 2);
    assert_eq!(m.pending_commit(), None);
    assert_eq!(m.poll(100), Ok(McsControlEvent::None));
}
