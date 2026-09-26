use vradm_core::arq::{ArqReceiver, IpPacketSlicer};

#[test]
fn later_complete_packet_waits_for_missing_earlier_packet() {
    let mut rx = ArqReceiver::new();
    let a = IpPacketSlicer::slice(&[0x41; 74], false, false, 0).unwrap();
    let b = IpPacketSlicer::slice(&[0x42; 19], false, false, 2).unwrap();
    assert_eq!(rx.receive_frame(&a[0]), None);
    assert_eq!(rx.receive_frame(&b[0]), None, "later packet must be held");
    assert_eq!((rx.ack_base, rx.ack_map), (0, 2));
    assert_eq!(rx.receive_frame(&a[1]), Some(vec![0x41; 74]));
}

fn poll(rx: &mut ArqReceiver) -> Option<(Vec<u8>, bool)> {
    let mut out = [0; 296];
    rx.poll_packet_into(&mut out).map(|(len, be)| (out[..len].to_vec(), be))
}

#[test]
fn one_recovered_gap_releases_multiple_packets_in_order_across_wrap() {
    for initial in [0u8, 253, 255] {
        let mut rx = ArqReceiver::with_initial_seq(initial);
        let packets: Vec<_> = (0..8u8).map(|i|
            IpPacketSlicer::slice(&[i; 19], false, false, initial.wrapping_add(i)).unwrap()[0]).collect();
        for i in (1..8).rev() {
            rx.ingest_frame(&packets[i]);
            assert!(poll(&mut rx).is_none());
        }
        assert_eq!(rx.ack_base, initial.wrapping_sub(1));
        rx.ingest_frame(&packets[0]);
        assert_eq!(rx.ack_base, initial.wrapping_add(7));
        for i in 0..8u8 { assert_eq!(poll(&mut rx), Some((vec![i; 19], false))); }
        for frame in &packets { rx.ingest_frame(frame); }
        assert!(poll(&mut rx).is_none());
    }
}

#[test]
fn best_effort_bypasses_gap_without_evicting_held_reliable_data() {
    let mut rx = ArqReceiver::new();
    let later = IpPacketSlicer::slice(&[0x42; 19], false, false, 1).unwrap()[0];
    rx.ingest_frame(&later);
    for i in 0..100u8 {
        let be = IpPacketSlicer::slice(&[i; 19], false, true, i).unwrap()[0];
        rx.ingest_frame(&be);
        assert_eq!(poll(&mut rx), Some((vec![i; 19], true)));
    }
    assert_eq!((rx.ack_base, rx.ack_map), (255, 2));
    rx.ingest_frame(&IpPacketSlicer::slice(&[0x41; 19], false, false, 0).unwrap()[0]);
    assert_eq!(poll(&mut rx), Some((vec![0x41; 19], false)));
    assert_eq!(poll(&mut rx), Some((vec![0x42; 19], false)));
}

#[test]
fn unpolled_completed_packets_are_bounded_and_never_evicted() {
    let mut rx = ArqReceiver::new();
    // All sixteen slots remain occupied until the downstream consumer drains.
    for i in 0..16u8 {
        rx.ingest_frame(&IpPacketSlicer::slice(&[i; 19], false, false, i).unwrap()[0]);
    }
    assert_eq!(rx.ack_base, 15);
    let blocked = IpPacketSlicer::slice(&[16; 19], false, false, 16).unwrap()[0];
    rx.ingest_frame(&blocked);
    assert_eq!(rx.ack_base, 15);
    for i in 0..40u8 {
        rx.ingest_frame(&IpPacketSlicer::slice(&[i; 19], false, true, i).unwrap()[0]);
    }
    for i in 0..16u8 { assert_eq!(poll(&mut rx), Some((vec![i; 19], false))); }
    rx.ingest_frame(&blocked);
    assert_eq!(poll(&mut rx), Some((vec![16; 19], false)));
    assert_eq!(rx.ack_base, 16);
}

#[test]
fn reliable_keepalives_advance_order_without_delivering_empty_packets() {
    use vradm_core::framing::CanonicalDataFrame;
    let mut rx = ArqReceiver::new();
    let mut keepalive = CanonicalDataFrame::new();
    keepalive.seq = 1;
    rx.ingest_frame(&keepalive);
    rx.ingest_frame(&IpPacketSlicer::slice(&[2], false, false, 2).unwrap()[0]);
    assert!(poll(&mut rx).is_none());
    rx.ingest_frame(&IpPacketSlicer::slice(&[0], false, false, 0).unwrap()[0]);
    assert_eq!(poll(&mut rx), Some((vec![0], false)));
    assert_eq!(poll(&mut rx), Some((vec![2], false)));
    assert!(poll(&mut rx).is_none());
    assert_eq!(rx.next_delivery_seq, 3);
    rx.reset();
    assert_eq!(rx.next_delivery_seq, 0);
    assert!(poll(&mut rx).is_none());
}

#[test]
fn overlapping_packet_identity_does_not_poison_order_or_ack_state() {
    let mut rx = ArqReceiver::new();
    rx.ingest_frame(&IpPacketSlicer::slice(&[1], false, false, 1).unwrap()[0]);
    // Claims that seq 1 belongs to a different two-fragment packet starting at 0.
    let overlap = IpPacketSlicer::slice(&[9; 74], false, false, 0).unwrap();
    rx.ingest_frame(&overlap[0]);
    assert_eq!((rx.ack_base, rx.ack_map), (255, 2));
    rx.ingest_frame(&IpPacketSlicer::slice(&[0], false, false, 0).unwrap()[0]);
    assert_eq!(poll(&mut rx), Some((vec![0], false)));
    assert_eq!(poll(&mut rx), Some((vec![1], false)));
}

#[test]
fn old_packet_cannot_gain_new_fragments_after_dedup_context_eviction() {
    let mut rx = ArqReceiver::new();
    rx.ingest_frame(&IpPacketSlicer::slice(&[0], false, false, 0).unwrap()[0]);
    assert_eq!(poll(&mut rx), Some((vec![0], false)));
    for i in 0..80u8 {
        rx.ingest_frame(&IpPacketSlicer::slice(&[i], false, true, i).unwrap()[0]);
        assert!(poll(&mut rx).is_some());
    }
    let bogus = IpPacketSlicer::slice(&[9; 74], false, false, 0).unwrap();
    rx.ingest_frame(&bogus[1]); // seq 1, but packet 0 has already been delivered
    assert_eq!(rx.ack_base, 0);
    rx.ingest_frame(&IpPacketSlicer::slice(&[1], false, false, 1).unwrap()[0]);
    assert_eq!(poll(&mut rx), Some((vec![1], false)));
}
