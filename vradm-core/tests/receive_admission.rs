use vradm_core::arq::{ArqReceiver, IpPacketSlicer};
use vradm_core::framing::CanonicalDataFrame;

fn frames(packet: &[u8], best_effort: bool, seq: u8) -> Vec<CanonicalDataFrame> {
    IpPacketSlicer::slice(packet, false, best_effort, seq).unwrap()
}

#[test]
fn invalid_fragment_does_not_change_receiver_state() {
    let valid = frames(&[7; 74], false, 0)[0];
    for defect in 0..6 {
        let mut rx = ArqReceiver::new();
        let mut bad = valid;
        match defect {
            0 => bad.payload_len = 255,
            1 => bad.payload[0] |= 0x70, // index 7 in a two-fragment packet
            2 => bad.payload[0] |= 1,    // fragment class disagrees with frame class
            3 => bad.ctrl |= 0x80,       // SOTP is not an IP fragment
            4 => bad.ctrl = 0,           // unsupported version
            _ => bad.ctrl = 0x72,        // reserved MCS
        }
        assert_eq!(rx.receive_frame(&bad), None);
        assert_eq!((rx.ack_base, rx.ack_map, rx.recv_bitmap), (255, 0, 0));
        assert!(!rx.initialized);
        assert!(rx.contexts.iter().all(Option::is_none));
        assert_eq!(rx.receive_frame(&valid), None);
        assert_eq!(rx.ack_base, 0);
    }
}

#[test]
fn conflicting_fragment_count_is_not_acknowledged() {
    let packet = [9; 74];
    let valid = frames(&packet, false, 0);
    let mut rx = ArqReceiver::new();
    assert_eq!(rx.receive_frame(&valid[0]), None);
    let mut conflicting = valid[1];
    conflicting.payload[0] = (conflicting.payload[0] & !0x0e) | 4; // claims three fragments
    assert_eq!(rx.receive_frame(&conflicting), None);
    assert_eq!(rx.ack_base, 0);
    assert_eq!(rx.receive_frame(&valid[1]).as_deref(), Some(packet.as_slice()));
    assert_eq!(rx.ack_base, 1);
}

#[test]
fn best_effort_pressure_cannot_evict_acknowledged_reliable_fragments() {
    let packet = [0x51; 74];
    let reliable = frames(&packet, false, 0);
    let mut rx = ArqReceiver::new();
    assert_eq!(rx.receive_frame(&reliable[0]), None);
    assert_eq!(rx.ack_base, 0);
    // Incomplete best-effort datagrams fill all available contexts.
    for i in 0..40 {
        let unreliable = frames(&[0x33; 74], true, i * 2);
        assert_eq!(rx.receive_frame(&unreliable[0]), None);
    }
    assert_eq!(rx.receive_frame(&reliable[1]).as_deref(), Some(packet.as_slice()));
}

#[test]
fn frame_outside_reliable_window_cannot_poison_reassembly() {
    let mut rx = ArqReceiver::new();
    assert!(rx.receive_frame(&frames(&[1], false, 0)[0]).is_some());
    let before = rx.contexts;
    assert_eq!(rx.receive_frame(&frames(&[2], false, 9)[0]), None);
    assert_eq!((rx.ack_base, rx.ack_map, rx.current_seq_rx), (0, 0, 0));
    assert_eq!(rx.contexts, before);
}

#[test]
fn losing_entire_first_packet_does_not_acknowledge_it() {
    use vradm_core::arq::ArqTransmitter;
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();
    tx.enqueue_packet(&[0x41; 19], false, false).unwrap();
    tx.enqueue_packet(&[0x42; 19], false, false).unwrap();
    let sent = tx.get_frames_to_transmit(8);
    assert_eq!(rx.receive_frame(&sent[1]), None);
    assert_eq!((rx.ack_base, rx.ack_map), (255, 2));
    tx.on_ack_received(rx.ack_base, rx.ack_map);
    let retry = tx.get_frames_to_transmit(8);
    assert_eq!(retry, vec![sent[0]], "first packet must remain eligible for retry");
    assert_eq!(rx.receive_frame(&retry[0]), Some(vec![0x41; 19]));
    tx.on_ack_received(rx.ack_base, rx.ack_map);
    assert!(tx.in_flight.is_empty());
    let mut out = [0; 296];
    assert_eq!(rx.poll_packet_into(&mut out), Some((19, false)));
    assert_eq!(&out[..19], &[0x42; 19]);
    assert_eq!(rx.receive_frame(&sent[1]), None);
}

#[test]
fn first_far_ahead_packet_cannot_choose_the_receive_window() {
    let mut rx = ArqReceiver::new();
    assert_eq!(rx.receive_frame(&frames(&[0x61], false, 80)[0]), None);
    assert_eq!((rx.ack_base, rx.ack_map), (255, 0));
    assert!(!rx.initialized);
    assert!(rx.contexts.iter().all(Option::is_none));
    assert_eq!(rx.receive_frame(&frames(&[0x62], false, 0)[0]), Some(vec![0x62]));
}

#[test]
fn explicit_initial_sequence_handles_wrap_and_reset() {
    let mut rx = ArqReceiver::with_initial_seq(255);
    assert_eq!(rx.receive_frame(&frames(&[2], false, 0)[0]), None);
    assert_eq!((rx.ack_base, rx.ack_map), (254, 2));
    assert_eq!(rx.receive_frame(&frames(&[1], false, 255)[0]), Some(vec![1]));
    assert_eq!((rx.ack_base, rx.ack_map), (0, 0));
    let mut out = [0; 296];
    assert_eq!(rx.poll_packet_into(&mut out), Some((1, false)));
    assert_eq!(out[0], 2);
    rx.reset();
    assert_eq!((rx.ack_base, rx.current_seq_rx), (255, 255));
    assert!(!rx.initialized);
    assert!(rx.contexts.iter().all(Option::is_none));
    // The first post-reset packet cannot silently acknowledge missing seq 0.
    assert_eq!(rx.receive_frame(&frames(&[4], false, 1)[0]), None);
    assert_eq!(rx.ack_base, 255);
    assert_eq!(rx.receive_frame(&frames(&[3], false, 0)[0]), Some(vec![3]));
    assert_eq!(rx.ack_base, 1);
}
