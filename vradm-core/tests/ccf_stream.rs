use vradm_core::{ccf_stream::*, ccf_turn::*, framing::CompactControlFrame};
fn wire() -> [u8; 16] {
    CompactControlFrame {
        ccf_ctrl: 0xa9,
        ack_base: 37,
        ack_map: 5,
        ccf_mac: 19,
    }
    .encode()
}
fn turn() -> Vec<i16> {
    let mut tx = CcfTurnTransmitter::new();
    tx.start(wire()).unwrap();
    let mut pcm = vec![0; CCF_TURN_SAMPLES];
    tx.render(&mut pcm);
    pcm
}
#[test]
fn arbitrary_leading_offset_and_chunking_acquire_without_extra_preamble() {
    for offset in [0, 1, 17, 39, 40, 79, 137, 399, 511, 1001] {
        let mut pcm = vec![0; offset];
        pcm.extend(turn());
        for size in [79, 160, 511] {
            let mut rx = CcfStreamReceiver::new();
            let mut found = 0;
            for chunk in pcm.chunks(size) {
                let mut pos = 0;
                while pos < chunk.len() {
                    let result = rx.push(&chunk[pos..]);
                    pos += result.consumed;
                    if let Some(frame) = result.frame {
                        assert_eq!(
                            CompactControlFrame::decode(frame.codeword(), frame.erasures())
                                .unwrap(),
                            CompactControlFrame::decode(wire(), &[]).unwrap()
                        );
                        found += 1;
                    }
                }
            }
            assert_eq!(found, 1, "offset {offset}, chunk {size}");
        }
    }
}
#[test]
fn silence_and_partial_capture_reset_do_not_emit_frames() {
    let mut rx = CcfStreamReceiver::new();
    assert!(rx.push(&[0; 16000]).frame.is_none());
    let pcm = turn();
    assert!(rx.push(&pcm[..5000]).frame.is_none());
    rx.reset();
    assert!(rx.push(&pcm[5000..]).frame.is_none());
    rx.reset();
    assert!(rx.push(&pcm).frame.is_some());
}

#[test]
fn consecutive_turns_with_payload_erasure_keep_streaming() {
    let mut pcm = vec![0; 137];
    let mut damaged = turn();
    damaged[4000..4400].fill(0);
    pcm.extend(damaged);
    pcm.extend(turn());
    let mut rx = CcfStreamReceiver::new();
    let mut offset = 0;
    let mut count = 0;
    while offset < pcm.len() {
        let result = rx.push(&pcm[offset..]);
        offset += result.consumed;
        if let Some(frame) = result.frame {
            assert_eq!(
                CompactControlFrame::decode(frame.codeword(), frame.erasures()).unwrap(),
                CompactControlFrame::decode(wire(), &[]).unwrap()
            );
            count += 1;
        }
    }
    assert_eq!(count, 2);
}
