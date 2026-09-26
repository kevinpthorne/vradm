use vradm_core::crc::payload_crc16;
use vradm_core::framing::CompactControlFrame;
use vradm_core::security::*;

fn keys() -> SessionKeys {
    SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16])
}
fn repair_crc(wire: &mut BootstrapWire) {
    let crc = payload_crc16(&wire[..29]);
    wire[29..31].copy_from_slice(&crc.to_be_bytes());
}

#[test]
fn bootstrap_establishes_matching_keys_and_consumes_pending_nonce() {
    let psk = [1; 16];
    let mut initiator = PendingBootstrap::new(psk, [2; 16]);
    let wire = initiator.request().unwrap();
    assert_eq!(wire[0], 0xbe);
    assert_eq!(&wire[1..17], &[2; 16]);
    assert_eq!(&wire[17..21], &[0; 4]);
    assert_eq!(wire[31], 0);
    assert_eq!(
        initiator.request().unwrap(),
        wire,
        "retry must preserve request"
    );
    let request = VerifiedRequest::decode(&wire, &psk).unwrap();
    let (reply, responder) = request.accept(&psk, [3; 16]);
    assert_eq!(reply[0], 0xbf);
    assert_eq!(&reply[17..21], &responder.epoch().to_le_bytes());
    let initiator_keys = initiator.finish(&reply).unwrap();
    assert_eq!(initiator_keys.epoch(), responder.epoch());
    let mut tx = ControlTx::new(initiator_keys);
    let mut rx = ControlRx::new(responder, 0);
    assert_eq!(rx.verify_beacon(tx.beacon(3, 2, 1).unwrap(), 0), Ok(0));
    assert!(matches!(
        initiator.finish(&reply),
        Err(SecurityError::NoPendingRequest)
    ));
    assert_eq!(initiator.request(), Err(SecurityError::NoPendingRequest));
}

#[test]
fn accepts_cannot_cross_pending_transactions_or_wrong_psks() {
    let psk = [1; 16];
    let original = PendingBootstrap::new(psk, [2; 16]);
    let request = VerifiedRequest::decode(&original.request().unwrap(), &psk).unwrap();
    let (reply, _) = request.accept(&psk, [3; 16]);
    let mut different_nonce = PendingBootstrap::new(psk, [4; 16]);
    assert!(matches!(
        different_nonce.finish(&reply),
        Err(SecurityError::BadMac)
    ));
    assert!(different_nonce.request().is_ok());
    let mut wrong_psk = PendingBootstrap::new([9; 16], [2; 16]);
    assert!(matches!(
        wrong_psk.finish(&reply),
        Err(SecurityError::BadMac)
    ));
    assert!(matches!(
        VerifiedRequest::decode(&original.request().unwrap(), &[9; 16]),
        Err(SecurityError::BadMac)
    ));
}

#[test]
fn bootstrap_separates_channel_corruption_from_mac_and_structure_errors() {
    let pending = PendingBootstrap::new([1; 16], [2; 16]);
    let original = pending.request().unwrap();
    for byte in 0..31 {
        let mut wire = original;
        wire[byte] ^= 1;
        assert!(matches!(
            VerifiedRequest::decode(&wire, &[1; 16]),
            Err(SecurityError::ChannelIntegrity)
        ));
    }
    let mut wire = original;
    wire[21] ^= 1;
    repair_crc(&mut wire);
    assert!(matches!(
        VerifiedRequest::decode(&wire, &[1; 16]),
        Err(SecurityError::BadMac)
    ));
    for byte in [0, 17, 31] {
        let mut wire = original;
        wire[byte] ^= 1;
        repair_crc(&mut wire);
        assert!(matches!(
            VerifiedRequest::decode(&wire, &[1; 16]),
            Err(SecurityError::Malformed)
        ));
    }
}

#[test]
fn nonce_order_and_psk_are_bound_into_epoch() {
    let base = keys().epoch();
    assert_ne!(
        base,
        SessionKeys::derive(&[1; 16], &[3; 16], &[2; 16]).epoch()
    );
    assert_ne!(
        base,
        SessionKeys::derive(&[9; 16], &[2; 16], &[3; 16]).epoch()
    );
    assert_ne!(
        base,
        SessionKeys::derive(&[1; 16], &[2; 16], &[4; 16]).epoch()
    );
}

#[test]
fn all_session_counters_cross_wire_wrap_and_stop_before_reuse() {
    let mut tx = ControlTx::new(keys());
    let mut rx = ControlRx::new(keys(), 0);
    assert_eq!(tx.last_counter(), None);
    for counter in 0..REKEY_COUNTER {
        let beacon = tx.beacon(3, 2, 1).unwrap();
        assert_eq!(beacon.sequence, counter as u8);
        assert_eq!(rx.verify_beacon(beacon, counter as u64), Ok(counter));
        assert_eq!(tx.last_counter(), Some(counter));
    }
    for _ in 0..2 {
        assert_eq!(tx.beacon(3, 2, 1), Err(SecurityError::RekeyRequired));
    }
}

#[test]
fn replay_window_accepts_missing_recent_counters_but_never_duplicates() {
    let mut tx = ControlTx::new(keys());
    let beacons: Vec<_> = (0..=127).map(|_| tx.beacon(3, 2, 1).unwrap()).collect();
    let mut rx = ControlRx::new(keys(), 0);
    assert_eq!(rx.verify_beacon(beacons[0], 0), Ok(0));
    assert_eq!(rx.verify_beacon(beacons[0], 0), Err(SecurityError::Replay));
    assert_eq!(rx.verify_beacon(beacons[63], 0), Ok(63));
    assert_eq!(rx.verify_beacon(beacons[1], 0), Ok(1));
    assert_eq!(rx.verify_beacon(beacons[1], 0), Err(SecurityError::Replay));
    assert_eq!(rx.verify_beacon(beacons[64], 0), Ok(64));
    assert_eq!(rx.verify_beacon(beacons[0], 0), Err(SecurityError::Replay));
    assert_eq!(rx.verify_beacon(beacons[127], 0), Ok(127));
    assert_eq!(rx.verify_beacon(beacons[64], 0), Err(SecurityError::Replay));
    assert_eq!(rx.mac_failures(), 0);
}

#[test]
fn forged_future_beacon_cannot_advance_replay_window() {
    let mut tx = ControlTx::new(keys());
    let first = tx.beacon(3, 2, 1).unwrap();
    let mut future = first;
    for _ in 0..100 {
        future = tx.beacon(3, 2, 1).unwrap();
    }
    future.mac ^= 1;
    let mut rx = ControlRx::new(keys(), 0);
    assert_eq!(rx.verify_beacon(future, 0), Err(SecurityError::BadMac));
    assert_eq!(rx.verify_beacon(first, 0), Ok(0));
    assert_eq!(rx.mac_failures(), 1);
}

#[test]
fn ambiguous_or_underflow_counter_does_not_mutate_live_state() {
    let mut tx = ControlTx::new(keys());
    let first = tx.beacon(3, 2, 1).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    for sequence in [128, 255] {
        assert_eq!(
            rx.verify_beacon(Beacon { sequence, ..first }, 0),
            Err(SecurityError::CounterInference)
        );
    }
    assert_eq!(rx.verify_beacon(first, 0), Ok(0));
    assert_eq!(rx.mac_failures(), 0);
}

#[test]
fn failure_bucket_bounds_work_without_silencing_receiver_or_accepting_backwards_time() {
    let mut tx = ControlTx::new(keys());
    let valid = tx.beacon(3, 2, 1).unwrap();
    let invalid = Beacon {
        mac: valid.mac ^ 1,
        ..valid
    };
    let mut rx = ControlRx::new(keys(), 1000);
    for _ in 0..10 {
        assert_eq!(rx.verify_beacon(invalid, 1000), Err(SecurityError::BadMac));
    }
    assert_eq!(
        rx.verify_beacon(invalid, 1000),
        Err(SecurityError::RateLimited)
    );
    assert_eq!(
        rx.verify_beacon(invalid, 0),
        Err(SecurityError::RateLimited)
    );
    assert_eq!(
        rx.verify_beacon(invalid, 1099),
        Err(SecurityError::RateLimited)
    );
    assert_eq!(rx.mac_failures(), 10);
    assert_eq!(rx.verify_beacon(valid, 1100), Ok(0));
    assert_eq!(rx.mac_failures(), 10);
    // Success does not spend failure credit, so a subsequent bad candidate can be checked.
    let next = tx.beacon(3, 2, 1).unwrap();
    assert_eq!(
        rx.verify_beacon(
            Beacon {
                mac: next.mac ^ 1,
                ..next
            },
            1100
        ),
        Err(SecurityError::BadMac)
    );
}

#[test]
fn ccf_binds_full_counter_and_rejects_duplicates_after_wire_decode() {
    let frame = CompactControlFrame {
        ccf_ctrl: 0,
        ack_base: 11,
        ack_map: 3,
        ccf_mac: 0,
    };
    let wire = keys().sign_ccf(0, frame).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    assert!(matches!(
        rx.verify_ccf(wire, &[], 256, 0),
        Err(SecurityError::BadMac)
    ));
    let decoded = rx.verify_ccf(wire, &[], 0, 0).unwrap();
    assert_eq!(decoded.ack_base, 11);
    assert!(matches!(
        rx.verify_ccf(wire, &[], 0, 0),
        Err(SecurityError::Replay)
    ));
    assert_eq!(rx.mac_failures(), 1);
}

#[test]
fn ccf_channel_errors_and_correctable_noise_are_not_mac_failures() {
    let frame = CompactControlFrame {
        ccf_ctrl: 0,
        ack_base: 11,
        ack_map: 3,
        ccf_mac: 0,
    };
    let mut wire = keys().sign_ccf(0, frame).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    assert!(matches!(
        rx.verify_ccf([0; 16], &[], 0, 0),
        Err(SecurityError::ChannelIntegrity)
    ));
    assert_eq!(rx.mac_failures(), 0);
    for i in 0..4 {
        wire[i] ^= 0x57;
    }
    assert!(rx.verify_ccf(wire, &[], 0, 0).is_ok());
    assert_eq!(rx.mac_failures(), 0);
}

#[test]
fn trusted_outage_invalidates_both_control_paths_until_fresh_bootstrap() {
    let mut tx = ControlTx::new(keys());
    let beacon = tx.beacon(3, 2, 1).unwrap();
    let wire = keys().sign_ccf(0, CompactControlFrame::new()).unwrap();
    let mut rx = ControlRx::new(keys(), 0);
    rx.invalidate();
    assert_eq!(
        rx.verify_beacon(beacon, 1_000_000),
        Err(SecurityError::ResyncRequired)
    );
    assert!(matches!(
        rx.verify_ccf(wire, &[], 0, 1_000_000),
        Err(SecurityError::ResyncRequired)
    ));
    assert_eq!(rx.mac_failures(), 0);
}

#[test]
fn bootstrap_wire_fixture() {
    let mut initiator = PendingBootstrap::new([1; 16], [2; 16]);
    let request = initiator.request().unwrap();
    let verified = VerifiedRequest::decode(&request, &[1; 16]).unwrap();
    let (reply, _) = verified.accept(&[1; 16], [3; 16]);
    // Interoperability fixture for the documented LE64 tag convention. Generated
    // with blake3 1.8.2 / siphasher 1.0.3; not an official V-RADM test vector.
    assert_eq!(
        request,
        [
            0xbe, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0, 0, 0, 0xdc, 0x88, 0xf1,
            0x3c, 0xc8, 0x42, 0x8a, 0x36, 0x2a, 0x73, 0,
        ]
    );
    assert_eq!(
        reply,
        [
            0xbf, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 0x69, 0xff, 0x60, 0x70, 0x6f,
            0xc6, 0xf7, 0x86, 0x8a, 0xe5, 8, 0x72, 0x9e, 0xb4, 0,
        ]
    );
    assert_eq!(initiator.finish(&reply).unwrap().epoch(), 0x7060ff69);
}
