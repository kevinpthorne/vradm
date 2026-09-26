use vradm_core::security::*;
use vradm_core::session::*;

struct Source(u8);
impl NonceSource for Source {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        let result = [self.0; 16];
        self.0 += 1;
        Ok(result)
    }
}
struct Unavailable;
impl NonceSource for Unavailable {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        Err(SessionError::EntropyUnavailable)
    }
}
fn wire(event: SessionEvent) -> BootstrapWire {
    match event {
        SessionEvent::Transmit(wire) => wire,
        other => panic!("expected transmission, got {other:?}"),
    }
}
fn managers<const N: usize>() -> (SessionManager<N>, SessionManager<N>) {
    (
        SessionManager::new([1; 16], 0),
        SessionManager::new([1; 16], 0),
    )
}
fn establish<const N: usize>(
    a: &mut SessionManager<N>,
    b: &mut SessionManager<N>,
) -> (BootstrapWire, BootstrapWire) {
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    assert_eq!(
        a.receive(&reply, 0, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        b.verify_peer_beacon(a.beacon(3, 2, 1, 0).unwrap(), 0),
        Ok(0)
    );
    (request, reply)
}

#[test]
fn lost_accept_retries_same_transaction_then_active_requests_are_replays() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    assert!(!b.is_established(0).unwrap());
    let retry = wire(a.poll(6000).unwrap());
    assert_eq!(retry, request);
    assert_eq!(
        wire(
            b.receive(&retry, 6000, BootstrapMode::Fsk100, &mut Unavailable)
                .unwrap()
        ),
        reply
    );
    assert_eq!(
        a.receive(&reply, 6000, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
    let valid = a.beacon(3, 2, 1, 6000).unwrap();
    assert_eq!(
        b.verify_peer_beacon(
            Beacon {
                mac: valid.mac ^ 1,
                ..valid
            },
            6000
        ),
        Err(SessionError::Security(SecurityError::BadMac))
    );
    assert!(!b.is_established(6000).unwrap());
    assert_eq!(b.verify_peer_beacon(valid, 6000), Ok(0));
    assert!(b.is_established(6000).unwrap());
    assert_eq!(
        b.receive(&request, 6000, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Replay)
    );
    assert_eq!(
        a.receive(&reply, 6000, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::NoPendingTransaction)
    );
}

#[test]
fn both_modulation_profiles_retry_three_times_then_expire() {
    for (mode, times, end) in [
        (BootstrapMode::Fsk100, [6000, 15000, 28500], 42000),
        (BootstrapMode::Mcs0, [7500, 18750, 35625], 52500),
    ] {
        let mut a = SessionManager::<8>::new([1; 16], 0);
        let original = wire(a.begin(0, mode, &mut Source(2)).unwrap());
        for time in times {
            assert_eq!(a.poll(time - 1), Ok(SessionEvent::None));
            assert_eq!(a.poll(time), Ok(SessionEvent::Transmit(original)));
            assert_eq!(a.poll(time), Ok(SessionEvent::None));
        }
        assert_eq!(a.poll(end - 1), Ok(SessionEvent::None));
        assert_eq!(a.poll(end), Ok(SessionEvent::TimedOut));
        assert!(a.context(end).unwrap().is_none());
        assert_eq!(
            a.begin(end, mode, &mut Source(2)),
            Err(SessionError::NonceCollision)
        );
    }
}

#[test]
fn late_poll_does_not_emit_catchup_bursts_or_extend_total_lifetime() {
    let mut a = SessionManager::<8>::new([1; 16], 0);
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    assert_eq!(a.poll(20000), Ok(SessionEvent::Transmit(request)));
    assert_eq!(a.poll(20000), Ok(SessionEvent::None));
    assert_eq!(a.poll(42000), Ok(SessionEvent::TimedOut));
}

#[test]
fn simultaneous_initiation_converges_without_device_specific_roles() {
    let (mut a, mut b) = managers::<8>();
    let low = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let high = wire(b.begin(0, BootstrapMode::Fsk100, &mut Source(9)).unwrap());
    assert_eq!(
        b.receive(&low, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::None)
    );
    let reply = wire(
        a.receive(&high, 1, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    assert_eq!(
        b.receive(&reply, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        a.context(1).unwrap().unwrap().role(),
        SessionRole::Responder
    );
    assert_eq!(
        b.context(1).unwrap().unwrap().role(),
        SessionRole::Initiator
    );
    assert_eq!(
        a.verify_peer_beacon(b.beacon(3, 2, 1, 1).unwrap(), 1),
        Ok(0)
    );
    assert!(a.is_established(1).unwrap());
    assert_eq!(
        a.context(1).unwrap().unwrap().keys().epoch(),
        b.context(1).unwrap().unwrap().keys().epoch()
    );
}

#[test]
fn equal_nonce_tie_and_bad_frames_do_not_change_pending_transaction() {
    let (mut a, _) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    assert_eq!(
        a.receive(&request, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::None)
    );
    let mut corrupt = request;
    corrupt[21] ^= 1;
    assert_eq!(
        a.receive(&corrupt, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::ChannelIntegrity))
    );
    assert_eq!(a.poll(6000), Ok(SessionEvent::Transmit(request)));
}

#[test]
fn reset_retains_cache_and_cache_exhaustion_never_evicts_recent_nonces() {
    let (mut a, mut b) = managers::<2>();
    let (request, _) = establish(&mut a, &mut b);
    assert_eq!(
        b.begin(1, BootstrapMode::Fsk100, &mut Source(8)),
        Err(SessionError::CacheFull)
    );
    assert!(b.is_established(1).unwrap());
    b.reset(1).unwrap();
    assert_eq!(
        b.receive(&request, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Replay)
    );
    let fresh = PendingBootstrap::new([1; 16], [8; 16]).request().unwrap();
    assert_eq!(
        b.receive(&fresh, 1, BootstrapMode::Fsk100, &mut Source(9)),
        Err(SessionError::CacheFull)
    );
    let at = NONCE_RETENTION_MS + 1;
    assert!(matches!(
        b.receive(&fresh, at, BootstrapMode::Fsk100, &mut Source(9)),
        Ok(SessionEvent::Transmit(_))
    ));
}

#[test]
fn active_nonces_remain_protected_past_cache_age_and_after_retirement() {
    let (mut a, mut b) = managers::<8>();
    let (request, _) = establish(&mut a, &mut b);
    let time = NONCE_RETENTION_MS * 2;
    assert_eq!(
        b.receive(&request, time, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Replay)
    );
    b.reset(time).unwrap();
    assert_eq!(
        b.receive(
            &request,
            time + NONCE_RETENTION_MS - 1,
            BootstrapMode::Fsk100,
            &mut Unavailable
        ),
        Err(SessionError::Replay)
    );
    assert!(matches!(
        b.receive(
            &request,
            time + NONCE_RETENTION_MS,
            BootstrapMode::Fsk100,
            &mut Source(10)
        ),
        Ok(SessionEvent::Transmit(_))
    ));
}

#[test]
fn pending_responder_duplicates_do_not_extend_deadline() {
    let request = PendingBootstrap::new([1; 16], [2; 16]).request().unwrap();
    let mut b = SessionManager::<8>::new([1; 16], 0);
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    assert_eq!(
        b.receive(&request, 41999, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Transmit(reply))
    );
    assert_eq!(b.poll(42000), Ok(SessionEvent::TimedOut));
    assert_eq!(
        b.receive(&request, 42000, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Replay)
    );
}

#[test]
fn entropy_failure_and_backwards_time_preserve_active_session() {
    let (mut a, mut b) = managers::<8>();
    establish(&mut a, &mut b);
    let epoch = a.context(10).unwrap().unwrap().keys().epoch();
    assert_eq!(
        a.begin(10, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::EntropyUnavailable)
    );
    assert_eq!(
        a.begin(9, BootstrapMode::Fsk100, &mut Source(8)),
        Err(SessionError::ClockWentBackwards)
    );
    assert!(a.is_established(10).unwrap());
    assert_eq!(a.context(10).unwrap().unwrap().keys().epoch(), epoch);
}

#[test]
fn new_transaction_rejects_stale_accept_without_consuming_new_nonce() {
    let (mut a, mut b) = managers::<8>();
    let (_, old_reply) = establish(&mut a, &mut b);
    let request = wire(a.begin(1, BootstrapMode::Fsk100, &mut Source(8)).unwrap());
    assert_eq!(
        a.receive(&old_reply, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Replay)
    );
    assert!(!a.is_established(1).unwrap());
    let reply = wire(
        b.receive(&request, 1, BootstrapMode::Fsk100, &mut Source(9))
            .unwrap(),
    );
    assert_eq!(
        a.receive(&reply, 1, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
}

#[test]
fn outgoing_beacons_require_establishment_and_exhausted_counter_requires_rekey() {
    let (mut a, mut b) = managers::<8>();
    assert_eq!(a.beacon(3, 2, 1, 0), Err(SessionError::NotEstablished));
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    assert_eq!(b.beacon(3, 2, 1, 0), Err(SessionError::NotEstablished));
    a.receive(&reply, 0, BootstrapMode::Fsk100, &mut Unavailable)
        .unwrap();
    for counter in 0..REKEY_COUNTER {
        let beacon = a.beacon(3, 2, 1, 0).unwrap();
        assert_eq!(b.verify_peer_beacon(beacon, 0), Ok(counter));
    }
    assert_eq!(
        a.beacon(3, 2, 1, 0),
        Err(SessionError::Security(SecurityError::RekeyRequired))
    );
    let fresh = wire(a.begin(1, BootstrapMode::Fsk100, &mut Source(8)).unwrap());
    assert_eq!(a.beacon(3, 2, 1, 1), Err(SessionError::NotEstablished));
    let reply = wire(
        b.receive(&fresh, 1, BootstrapMode::Fsk100, &mut Source(9))
            .unwrap(),
    );
    a.receive(&reply, 1, BootstrapMode::Fsk100, &mut Unavailable)
        .unwrap();
    assert_eq!(
        b.verify_peer_beacon(a.beacon(3, 2, 1, 1).unwrap(), 1),
        Ok(0)
    );
}

#[test]
fn os_entropy_can_complete_a_host_side_bootstrap() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(
        a.begin(0, BootstrapMode::Fsk100, &mut OsNonceSource)
            .unwrap(),
    );
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut OsNonceSource)
            .unwrap(),
    );
    assert_eq!(
        a.receive(&reply, 0, BootstrapMode::Fsk100, &mut OsNonceSource),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        b.verify_peer_beacon(a.beacon(3, 2, 1, 0).unwrap(), 0),
        Ok(0)
    );
}

fn corrupt_bootstrap_mac(mut frame: BootstrapWire) -> BootstrapWire {
    frame[21] ^= 1;
    repair_bootstrap_crc(&mut frame);
    frame
}

fn repair_bootstrap_crc(frame: &mut BootstrapWire) {
    let crc = vradm_core::crc::payload_crc16(&frame[..29]);
    frame[29..31].copy_from_slice(&crc.to_be_bytes());
}

#[test]
fn bootstrap_failures_throttle_requests_without_consuming_entropy_or_state() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let forged = corrupt_bootstrap_mac(request);
    for expected in 1..=10 {
        assert_eq!(
            b.receive(&forged, 0, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::BadMac))
        );
        assert_eq!(b.bootstrap_mac_failures(), expected);
    }
    for time in [0, 99] {
        assert_eq!(
            b.receive(&request, time, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::RateLimited))
        );
        assert!(b.context(time).unwrap().is_none());
    }
    // Rate limiting does not create a silent protocol backoff. Clock refill alone
    // lets the valid request proceed; no entropy was requested by rejected input.
    let reply = wire(
        b.receive(&request, 100, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    for _ in 0..20 {
        assert_eq!(
            b.receive(&request, 100, BootstrapMode::Fsk100, &mut Unavailable),
            Ok(SessionEvent::Transmit(reply))
        );
    }
    assert_eq!(b.bootstrap_mac_failures(), 10);
    assert_eq!(
        a.receive(&reply, 100, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        b.verify_peer_beacon(a.beacon(2, 2, 0, 100).unwrap(), 100),
        Ok(0)
    );
}

#[test]
fn bootstrap_request_and_accept_failures_share_budget_and_preserve_retry() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let reply = wire(
        b.receive(&request, 0, BootstrapMode::Fsk100, &mut Source(3))
            .unwrap(),
    );
    for i in 0..10 {
        let forged = corrupt_bootstrap_mac(if i % 2 == 0 { request } else { reply });
        assert_eq!(
            a.receive(&forged, 0, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::BadMac))
        );
    }
    assert_eq!(
        a.receive(&reply, 99, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::RateLimited))
    );
    // Exhausted verification credit does not mute scheduled local retries.
    assert_eq!(a.poll(6000), Ok(SessionEvent::Transmit(request)));
    assert_eq!(
        a.receive(&reply, 6000, BootstrapMode::Fsk100, &mut Unavailable),
        Ok(SessionEvent::Established)
    );
    assert_eq!(a.bootstrap_mac_failures(), 10);
}

#[test]
fn bootstrap_channel_errors_do_not_spend_or_hide_behind_failure_budget() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let mut corrupt = request;
    corrupt[1] ^= 1; // Stale CRC.
    let mut malformed = request;
    malformed[31] = 1;
    for _ in 0..20 {
        assert_eq!(
            b.receive(&corrupt, 0, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::ChannelIntegrity))
        );
        assert_eq!(
            b.receive(&malformed, 0, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::Malformed))
        );
    }
    assert_eq!(b.bootstrap_mac_failures(), 0);
    for _ in 0..10 {
        assert_eq!(
            b.receive(
                &corrupt_bootstrap_mac(request),
                0,
                BootstrapMode::Fsk100,
                &mut Unavailable
            ),
            Err(SessionError::Security(SecurityError::BadMac))
        );
    }
    assert_eq!(
        b.receive(&corrupt, 0, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::ChannelIntegrity))
    );
    assert_eq!(
        b.receive(&malformed, 0, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::Malformed))
    );
    assert_eq!(b.bootstrap_mac_failures(), 10);
}

#[test]
fn bootstrap_reset_and_clock_errors_cannot_refill_failure_budget() {
    let (mut a, mut b) = managers::<8>();
    let request = wire(a.begin(0, BootstrapMode::Fsk100, &mut Source(2)).unwrap());
    let forged = corrupt_bootstrap_mac(request);
    for _ in 0..10 {
        assert_eq!(
            b.receive(&forged, 100, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::BadMac))
        );
    }
    b.reset(100).unwrap();
    assert_eq!(
        b.receive(&request, 99, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::ClockWentBackwards)
    );
    assert_eq!(
        b.receive(&request, 199, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::RateLimited))
    );
    assert_eq!(b.bootstrap_mac_failures(), 10);
    // A huge trusted time jump is capped at ten credits, not an unbounded burst.
    for _ in 0..10 {
        assert_eq!(
            b.receive(&forged, u64::MAX, BootstrapMode::Fsk100, &mut Unavailable),
            Err(SessionError::Security(SecurityError::BadMac))
        );
    }
    assert_eq!(
        b.receive(&forged, u64::MAX, BootstrapMode::Fsk100, &mut Unavailable),
        Err(SessionError::Security(SecurityError::RateLimited))
    );
    assert_eq!(b.bootstrap_mac_failures(), 20);
}
