use vradm_core::{framing::CanonicalDataFrame, phy::*, security::*};
fn fixture() -> (Beacon, CanonicalDataFrame) {
    let mut tx = ControlTx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]));
    let beacon = tx.beacon(3, 3, 0).unwrap();
    let mut frame = CanonicalDataFrame::new();
    frame.ctrl = 0x3e;
    (beacon, frame)
}

#[test]
fn mcs3_verification_waits_for_full_guard_and_happens_once() {
    let (beacon, frame) = fixture();
    let mut tx = PhyTransmitter::new(3);
    let pcm = tx.modulate_authenticated(beacon, &[frame], true).unwrap();
    let mut rx = PhyReceiver::new();
    let mut frames = [CanonicalDataFrame::new(); 8];
    let mut auth = ControlRx::new(SessionKeys::derive(&[1; 16], &[2; 16], &[3; 16]), 0);
    let mut attempts = 0;
    let mut verify = |beacon| {
        attempts += 1;
        auth.verify_beacon(beacon, 0).is_ok()
    };
    rx.ingest_samples(&pcm[..4520]); // 520 preamble + 4000, still short of MCS3 guard
    assert_eq!(rx.process_with_verifier(&mut frames, true, &mut verify), 0);
    rx.ingest_samples(&pcm[4520..4640]);
    assert_eq!(rx.process_with_verifier(&mut frames, true, &mut verify), 0);
    rx.ingest_samples(&pcm[4640..]);
    assert_eq!(rx.process_with_verifier(&mut frames, true, &mut verify), 1);
    assert_eq!(attempts, 1);
    assert_eq!(frames[0], frame);
}

#[test]
fn rejected_header_does_not_change_demodulator_mcs_or_emit_frames() {
    let (beacon, frame) = fixture();
    let mut tx = PhyTransmitter::new(3);
    let pcm = tx.modulate_authenticated(beacon, &[frame], true).unwrap();
    let mut rx = PhyReceiver::new();
    let mut frames = [CanonicalDataFrame::new(); 8];
    let mut attempts = 0;
    for chunk in pcm.chunks(160) {
        rx.ingest_samples(chunk);
        assert_eq!(
            rx.process_with_verifier(&mut frames, true, &mut |_| {
                attempts += 1;
                false
            }),
            0
        );
    }
    assert_eq!(attempts, 1);
    assert_eq!(rx.dqpsk.mcs, 2);
}

#[test]
fn physical_header_erasure_never_attempts_mac_verification() {
    let (beacon, frame) = fixture();
    let mut tx = PhyTransmitter::new(3);
    let mut pcm = tx
        .modulate_authenticated(beacon, &[frame], true)
        .unwrap()
        .to_vec();
    pcm[520..4360].fill(0);
    let mut rx = PhyReceiver::new();
    let mut frames = [CanonicalDataFrame::new(); 8];
    let mut attempts = 0;
    for chunk in pcm.chunks(160) {
        rx.ingest_samples(chunk);
        assert_eq!(
            rx.process_with_verifier(&mut frames, true, &mut |_| {
                attempts += 1;
                false
            }),
            0
        );
    }
    assert_eq!(attempts, 0);
}
