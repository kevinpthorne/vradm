use vradm_core::arq::{ArqTransmitter, ArqReceiver};
use vradm_core::framing::{CanonicalDataFrame, seq_advance};

fn main() {
    let mut tx = ArqTransmitter::new();
    let mut rx = ArqReceiver::new();

    // Enqueue 8 frames
    let p = vec![0; 296];
    tx.enqueue_packet(&p, false, false).unwrap();
    let frames = tx.get_frames_to_transmit(10);
    assert_eq!(frames.len(), 8);

    // Receive frame 0 (advances ack_base to 0)
    rx.receive_frame(&frames[0]);
    assert_eq!(rx.ack_base, 0);

    // Drop frame 1 (seq 1)
    // Receive frame 8? Wait, if 8 frames are sent, seq is 0 to 7.
    // Let's drop frame 1 (seq 1), receive frame 7 (seq 7).
    rx.receive_frame(&frames[7]);
    
    let diff = (7 - 0) as u8; // diff = 7. diff <= 7, bit_idx = 6, bit 6 is set.
    println!("ack_map after receiving seq 7: {:08b}", rx.ack_map);

    // Now let's say W_ARQ is 8, so we send 8 frames: 1 to 8. (ack_base = 0).
    // Let's create a frame with seq = 8.
    let mut frame8 = CanonicalDataFrame::new();
    frame8.seq = 8;
    frame8.payload_len = 1;
    frame8.payload[0] = 0; // frag_idx 0

    rx.receive_frame(&frame8);
    println!("ack_map after receiving seq 8: {:08b}", rx.ack_map);
}
