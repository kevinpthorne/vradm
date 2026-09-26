use vradm_core::bootstrap_phy::*;
use vradm_core::security::*;
use vradm_core::session::*;

fn request() -> BootstrapWire {
    PendingBootstrap::new([1; 16], [2; 16]).request().unwrap()
}
fn pcm(wire: BootstrapWire) -> Vec<i16> {
    let mut out = vec![0; BOOTSTRAP_SAMPLES];
    assert_eq!(
        BootstrapFskTransmitter::new(wire).render(&mut out),
        BOOTSTRAP_SAMPLES
    );
    out
}
fn decode(samples: &[i16], chunk_size: usize) -> Result<BootstrapWire, BootstrapDecodeError> {
    let mut receiver = BootstrapFskReceiver::at_frame_start();
    let mut result = None;
    for chunk in samples.chunks(chunk_size) {
        let progress = receiver.push(chunk);
        assert_eq!(progress.consumed, chunk.len());
        if progress.frame.is_some() {
            assert!(result.is_none(), "duplicate frame notification");
            result = progress.frame;
        }
    }
    result.expect("missing frame")
}

#[test]
fn payload_roundtrips_across_unaligned_callback_sizes() {
    for size in [1, 17, 79, 80, 160, 320, 511] {
        let mut tx = BootstrapFskTransmitter::new(request());
        let mut rx = BootstrapFskReceiver::at_frame_start();
        let mut out = vec![123; size];
        let mut emitted = 0;
        let mut decoded = None;
        while tx.remaining_samples() > 0 {
            let count = tx.render(&mut out);
            emitted += count;
            assert!(out[count..].iter().all(|&v| v == 0));
            let progress = rx.push(&out[..count]);
            assert_eq!(progress.consumed, count);
            if progress.frame.is_some() {
                decoded = progress.frame;
            }
        }
        assert_eq!(emitted, 20_480);
        assert_eq!(decoded, Some(Ok(request())));
        assert_eq!(tx.render(&mut out), 0);
        assert!(out.iter().all(|&v| v == 0));
        assert_eq!(rx.push(&out).consumed, 0);
        assert!(rx.push(&[]).frame.is_none());
    }
}

#[test]
fn rendering_is_independent_of_callback_boundaries_and_peak_bounded() {
    let contiguous = pcm(request());
    assert!(contiguous.iter().all(|&v| (v as i32).abs() <= 14746));
    let mut chunked = vec![0; BOOTSTRAP_SAMPLES];
    let mut tx = BootstrapFskTransmitter::new(request());
    assert_eq!(tx.render(&mut []), 0);
    for chunk in chunked.chunks_mut(113) {
        assert_eq!(tx.render(chunk), chunk.len());
    }
    assert_eq!(chunked, contiguous);
    // Byte BE begins 1,0: check each bit against the expected PLCP tone convention.
    for (bit, frequency) in [(0, 1200.0_f32), (1, 1600.0)] {
        for n in 0..80 {
            let expected = ((2.0 * std::f32::consts::PI * frequency * n as f32 / 8000.0).cos()
                * 0.45
                * i16::MAX as f32)
                .round() as i16;
            assert_eq!(contiguous[bit * 80 + n], expected);
        }
    }
}

#[test]
fn receive_stops_before_following_frame_and_can_be_rearmed() {
    let samples = pcm(request());
    let mut two = samples.clone();
    two.extend_from_slice(&samples);
    let mut rx = BootstrapFskReceiver::at_frame_start();
    let first = rx.push(&two);
    assert_eq!(first.consumed, BOOTSTRAP_SAMPLES);
    assert_eq!(first.frame, Some(Ok(request())));
    rx.reset_to_frame_start();
    let second = rx.push(&two[first.consumed..]);
    assert_eq!(second.frame, first.frame);
    assert_eq!(second.consumed, BOOTSTRAP_SAMPLES);
    rx.reset_to_frame_start();
    assert!(rx.push(&samples[..23]).frame.is_none());
    rx.reset_to_frame_start();
    assert_eq!(rx.push(&samples).frame, Some(Ok(request())));
}

#[test]
fn erased_symbols_and_crc_corruption_never_yield_a_frame() {
    let mut samples = pcm(request());
    samples[80 * 30..80 * 31].fill(0);
    assert_eq!(decode(&samples, 160), Err(BootstrapDecodeError::Erasure));
    assert_eq!(
        decode(&vec![0; BOOTSTRAP_SAMPLES], 160),
        Err(BootstrapDecodeError::Erasure)
    );
    let mut wire = request();
    wire[1] ^= 1;
    assert_eq!(decode(&pcm(wire), 160), Err(BootstrapDecodeError::Crc));
    let mut partial = BootstrapFskReceiver::at_frame_start();
    assert!(partial
        .push(&samples[..BOOTSTRAP_SAMPLES - 1])
        .frame
        .is_none());
}

#[test]
fn inversion_and_attenuation_preserve_noncoherent_decode() {
    let mut samples = pcm(request());
    for sample in &mut samples {
        *sample = -*sample / 4;
    }
    assert_eq!(decode(&samples, 173), Ok(request()));
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
        _ => panic!("expected wire frame"),
    }
}

#[test]
fn request_and_accept_cross_pcm_before_session_establishment() {
    let mut a = SessionManager::<8>::new([1; 16], 0);
    let mut b = SessionManager::<8>::new([1; 16], 0);
    let request = transmitted(a.begin(0, BootstrapMode::Fsk100, &mut Fixed(2)).unwrap());
    let decoded = decode(&pcm(request), 160).unwrap();
    let reply = transmitted(
        b.receive(&decoded, 2560, BootstrapMode::Fsk100, &mut Fixed(3))
            .unwrap(),
    );
    let decoded = decode(&pcm(reply), 320).unwrap();
    assert_eq!(
        a.receive(&decoded, 5120, BootstrapMode::Fsk100, &mut Fixed(4)),
        Ok(SessionEvent::Established)
    );
    assert_eq!(
        a.context(5120).unwrap().unwrap().keys().epoch(),
        b.context(5120).unwrap().unwrap().keys().epoch()
    );
    // This test transports only bootstrap PCM: the responder still needs an
    // authenticated peer PLCP beacon before becoming established.
    assert!(!b.is_established(5120).unwrap());
}

#[test]
fn physical_crc_success_does_not_bypass_host_mac_verification() {
    let mut forged = request();
    forged[21] ^= 1;
    let crc = vradm_core::crc::payload_crc16(&forged[..29]);
    forged[29..31].copy_from_slice(&crc.to_be_bytes());
    let decoded = decode(&pcm(forged), 160).unwrap();
    let mut receiver = SessionManager::<8>::new([1; 16], 0);
    assert_eq!(
        receiver.receive(&decoded, 2560, BootstrapMode::Fsk100, &mut Fixed(3)),
        Err(SessionError::Security(SecurityError::BadMac))
    );
    assert!(receiver.context(2560).unwrap().is_none());
}
