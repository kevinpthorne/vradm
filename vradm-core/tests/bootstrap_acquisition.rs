use vradm_core::bootstrap_phy::*;
use vradm_core::security::*;
use vradm_core::session::*;

fn request() -> BootstrapWire {
    PendingBootstrap::new([1; 16], [2; 16]).request().unwrap()
}
fn pcm(wire: BootstrapWire) -> Vec<i16> {
    let mut out = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    assert_eq!(
        BarkerBootstrapTransmitter::new(wire).render(&mut out),
        out.len()
    );
    out
}
fn collect(samples: &[i16], chunk_size: usize) -> Vec<Result<BootstrapWire, BootstrapDecodeError>> {
    let mut receiver = BarkerBootstrapReceiver::new();
    let mut events = Vec::new();
    for chunk in samples.chunks(chunk_size) {
        let mut offset = 0;
        while offset < chunk.len() {
            let progress = receiver.push(&chunk[offset..]);
            assert!(progress.consumed > 0);
            offset += progress.consumed;
            if let Some(event) = progress.frame {
                events.push(event);
            }
        }
    }
    events
}

#[test]
fn acquires_every_symbol_phase_without_callback_alignment() {
    let frame = pcm(request());
    for offset in 0..80 {
        let mut samples = vec![0; 521 + offset];
        samples.extend_from_slice(&frame);
        let size = [1, 17, 79, 160, 320, 511][offset % 6];
        assert_eq!(
            collect(&samples, size),
            vec![Ok(request())],
            "offset={offset}, chunk={size}"
        );
    }
}

#[test]
fn transmitter_preserves_bare_payload_and_callback_independence() {
    let frame = pcm(request());
    assert_eq!(frame.len(), 21_000);
    let mut bare = vec![0; BOOTSTRAP_SAMPLES];
    BootstrapFskTransmitter::new(request()).render(&mut bare);
    assert_eq!(&frame[520..], &bare);
    let mut tx = BarkerBootstrapTransmitter::new(request());
    let mut chunked = vec![0; BARKER_BOOTSTRAP_SAMPLES];
    for chunk in chunked.chunks_mut(113) {
        assert_eq!(tx.render(chunk), chunk.len());
    }
    assert_eq!(chunked, frame);
    let mut out = [1; 160];
    assert_eq!(tx.render(&mut out), 0);
    assert_eq!(out, [0; 160]);
}

#[test]
fn local_peak_lookahead_survives_a_callback_ending_at_preamble() {
    let samples = pcm(request());
    let mut rx = BarkerBootstrapReceiver::new();
    assert!(rx.push(&samples[..520]).frame.is_none());
    for sample in &samples[520..523] {
        assert!(rx.push(&[*sample]).frame.is_none());
    }
    let end = rx.push(&samples[523..]);
    assert_eq!(end.consumed, samples.len() - 523);
    assert_eq!(end.frame, Some(Ok(request())));
}

#[test]
fn handles_polarity_gain_and_low_amplitude_background_noise() {
    let frame = pcm(request());
    for polarity in [-1, 1] {
        let mut state = 0x3712u32;
        let mut noise = || {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            ((state >> 16) % 101) as i16 - 50
        };
        let mut samples: Vec<i16> = (0..1027).map(|_| noise()).collect();
        samples.extend(
            frame
                .iter()
                .map(|&sample| polarity * (sample / 4) + noise()),
        );
        assert_eq!(collect(&samples, 173), vec![Ok(request())]);
    }
}

#[test]
fn corrupt_frame_does_not_block_following_frame_or_consume_its_preamble() {
    let frame = pcm(request());
    let mut samples = frame.clone();
    samples[520 + 80 * 30..520 + 80 * 31].fill(0);
    samples.extend_from_slice(&frame);
    assert_eq!(
        collect(&samples, samples.len()),
        vec![Err(BootstrapDecodeError::Erasure), Ok(request())]
    );
}

#[test]
fn reset_recovers_from_partial_frames_and_silence_has_no_events() {
    assert!(collect(&vec![0; 4096], 160).is_empty());
    let frame = pcm(request());
    let mut rx = BarkerBootstrapReceiver::new();
    assert!(rx.push(&frame[..1000]).frame.is_none());
    rx.reset();
    assert_eq!(rx.push(&frame).frame, Some(Ok(request())));
    assert!(rx.push(&[]).frame.is_none());
}

struct Fixed(u8);
impl NonceSource for Fixed {
    fn nonce(&mut self) -> Result<[u8; 16], SessionError> {
        Ok([self.0; 16])
    }
}
fn transmitted(event: SessionEvent) -> BootstrapWire {
    match event {
        SessionEvent::Transmit(wire) => wire,
        _ => panic!("expected transmission"),
    }
}

#[test]
fn request_and_accept_are_acquired_after_unannounced_leading_samples() {
    let mut a = SessionManager::<8>::new([1; 16], 0);
    let mut b = SessionManager::<8>::new([1; 16], 0);
    let request = transmitted(a.begin(0, BootstrapMode::Fsk100, &mut Fixed(2)).unwrap());
    let mut request_audio = vec![0; 133];
    request_audio.extend(pcm(request));
    let decoded = collect(&request_audio, 160)[0].unwrap();
    let reply = transmitted(
        b.receive(&decoded, 2800, BootstrapMode::Fsk100, &mut Fixed(3))
            .unwrap(),
    );
    let mut reply_audio = vec![0; 271];
    reply_audio.extend(pcm(reply));
    let decoded = collect(&reply_audio, 113)[0].unwrap();
    assert_eq!(
        a.receive(&decoded, 5600, BootstrapMode::Fsk100, &mut Fixed(4)),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        a.context(5600).unwrap().unwrap().keys().epoch(),
        b.context(5600).unwrap().unwrap().keys().epoch()
    );
    // Peer PLCP confirmation remains outside this bootstrap acquisition test.
    assert!(!b.is_established(5600).unwrap());
}
