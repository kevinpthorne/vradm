//! Software PCM links, independent of whether either host is a phone or PBX.
use vradm_core::c_abi::*;

struct Endpoint(*mut vradm_engine_t);
impl Endpoint {
    fn new(mcs: u8) -> Self {
        let config = vradm_config_t {
            sample_rate: VRADM_RATE_8K, startup_mcs: mcs, auto_rate_adaptation: 0,
            reserved: [0; 2], tx_amplitude: 0.1778, reserved2: [0; 4], psk_key: [42; 16],
        };
        let engine = unsafe { vradm_create(&config) };
        assert!(!engine.is_null());
        Self(engine)
    }
    fn write(&self, bytes: &[u8]) {
        assert_eq!(unsafe { vradm_write_ip_packet(self.0, bytes.as_ptr(), bytes.len() as u32) }, VRADM_OK);
    }
    fn poll(&self) -> Option<Vec<u8>> {
        let mut bytes = [0; 296];
        let len = unsafe { vradm_poll_ip_packet(self.0, bytes.as_mut_ptr(), 296) };
        assert!(len >= 0);
        (len > 0).then(|| bytes[..len as usize].to_vec())
    }
    fn sent(&self) -> u32 {
        let mut t = unsafe { std::mem::zeroed() };
        unsafe { vradm_get_telemetry(self.0, &mut t) };
        t.frames_transmitted
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) { unsafe { vradm_destroy(self.0) }; }
}
fn tick(a: &Endpoint, b: &Endpoint, count: usize, drop_ab: bool, drop_ba: bool) {
    let mut ab = [0; 320];
    let mut ba = [0; 320];
    unsafe {
        assert_eq!(vradm_generate_audio(a.0, ab.as_mut_ptr(), count as u32), count as u32);
        assert_eq!(vradm_generate_audio(b.0, ba.as_mut_ptr(), count as u32), count as u32);
        if drop_ab { ab.fill(0); }
        if drop_ba { ba.fill(0); }
        vradm_process_audio(b.0, ab.as_ptr(), count as u32);
        vradm_process_audio(a.0, ba.as_ptr(), count as u32);
    }
}

#[test]
fn symmetric_endpoints_sustain_bidirectional_traffic() {
    for mcs in [VRADM_MCS_2, VRADM_MCS_3] {
        let a = Endpoint::new(mcs);
        let b = Endpoint::new(mcs);
        // At least 256 frames in each direction, mixing one and eight fragments.
        for packet in 0..80 {
            let len = if packet % 2 == 0 { 296 } else { 19 };
            let ab = vec![packet as u8; len];
            let ba = vec![255 - packet as u8; len];
            a.write(&ab);
            b.write(&ba);
            let (mut got_a, mut got_b) = (false, false);
            for cycle in 0..1600 {
                tick(&a, &b, [160, 320, 80][cycle % 3], false, false);
                if let Some(bytes) = a.poll() { assert!(!got_a); assert_eq!(bytes, ba); got_a = true; }
                if let Some(bytes) = b.poll() { assert!(!got_b); assert_eq!(bytes, ab); got_b = true; }
                if got_a && got_b { break; }
            }
            assert!(got_a && got_b, "MCS {mcs}, packet {packet}");
        }
        for _ in 0..1500 { tick(&a, &b, 160, false, false); assert!(a.poll().is_none()); assert!(b.poll().is_none()); }
        let counts = (a.sent(), b.sent());
        for _ in 0..1500 { tick(&a, &b, 160, false, false); }
        assert_eq!((a.sent(), b.sent()), counts, "ACKs must not elicit ACKs");
    }
}

#[test]
fn lost_data_or_feedback_recovers_without_another_host_write() {
    for lose_feedback in [false, true] {
        let a = Endpoint::new(VRADM_MCS_2);
        let b = Endpoint::new(VRADM_MCS_2);
        let packet = [0x71; 256];
        a.write(&packet);
        let mut deliveries = 0;
        for cycle in 0..2500 {
            // A seven-frame burst takes ~5.2s. Lose either that whole burst,
            // or all feedback for the first 8s, then restore the channel.
            tick(&a, &b, 160, !lose_feedback && cycle < 270, lose_feedback && cycle < 400);
            if let Some(bytes) = b.poll() { assert_eq!(bytes, packet); deliveries += 1; }
        }
        assert_eq!(deliveries, 1, "retransmissions must not duplicate delivery");
        let sent = a.sent();
        assert!(sent > 7, "a retransmission must have occurred");
        for _ in 0..1000 { tick(&a, &b, 160, false, false); assert!(b.poll().is_none()); }
        assert_eq!(a.sent(), sent, "feedback must clear the in-flight window");
    }
}

#[test]
fn stalled_host_receives_every_reliable_packet_after_resuming() {
    let a = Endpoint::new(VRADM_MCS_3);
    let b = Endpoint::new(VRADM_MCS_3);
    // Fill the real host queue through PCM, without polling the receiver.
    for i in 0..64 {
        a.write(&[i; 19]);
        for _ in 0..110 { tick(&a, &b, 160, false, false); }
    }
    // The next datagram must remain unacknowledged while host capacity is zero.
    a.write(&[64; 256]);
    b.write(&[0x39; 19]); // reverse traffic must still progress while B is full
    for _ in 0..600 { tick(&a, &b, 160, false, false); }
    assert_eq!(a.poll(), Some(vec![0x39; 19]));
    for i in 0..64 { assert_eq!(b.poll(), Some(vec![i; 19])); }
    let mut received = None;
    for _ in 0..1000 {
        tick(&a, &b, 160, false, false);
        if let Some(packet) = b.poll() { assert!(received.is_none()); received = Some(packet); }
    }
    assert_eq!(received, Some(vec![64; 256]));
    let sent = a.sent();
    for _ in 0..1000 { tick(&a, &b, 160, false, false); assert!(b.poll().is_none()); }
    assert_eq!(a.sent(), sent);
}

#[test]
fn short_poll_buffer_preserves_packet_for_retry() {
    let a = Endpoint::new(VRADM_MCS_3);
    let b = Endpoint::new(VRADM_MCS_3);
    a.write(&[0x73; 200]);
    for _ in 0..300 { tick(&a, &b, 160, false, false); }
    let mut small = [0; 10];
    for _ in 0..2 {
        assert_eq!(unsafe { vradm_poll_ip_packet(b.0, small.as_mut_ptr(), 10) }, VRADM_ERR_BUFFER_TOO_SMALL);
    }
    assert_eq!(b.poll(), Some(vec![0x73; 200]));
    assert!(b.poll().is_none());
}

#[test]
fn pcm_gap_recovery_preserves_order_when_host_capacity_runs_out() {
    use vradm_core::arq::IpPacketSlicer;
    use vradm_core::phy::PhyTransmitter;
    let b = Endpoint::new(VRADM_MCS_3);
    let mut waveform = PhyTransmitter::new(VRADM_MCS_3);
    let mut feed = |seq: u8, value: u8| {
        let mut frames = IpPacketSlicer::slice(&[value; 19], false, false, seq).unwrap();
        frames[0].ctrl = 0x3a; // MCS 3, yield, reliable, v3.8
        let pcm = waveform.modulate_burst(3, &frames, seq.wrapping_add(1));
        for chunk in pcm.chunks(160) {
            unsafe { vradm_process_audio(b.0, chunk.as_ptr(), chunk.len() as u32) };
        }
    };
    // Populate 63 host slots via actual encoded PCM.
    for seq in 0..63 { feed(seq, seq); }
    // Seq 63 was erased on the first attempt. Later seq 64 is decoded first.
    feed(64, 64);
    feed(63, 63); // retransmission fills the last host slot; seq 64 stays held
    for seq in 0..64 { assert_eq!(b.poll(), Some(vec![seq; 19])); }
    assert!(b.poll().is_none());
    // No more incoming audio. The render callback must still flush seq 64.
    let mut silence = [0; 160];
    unsafe { vradm_generate_audio(b.0, silence.as_mut_ptr(), 160) };
    assert_eq!(b.poll(), Some(vec![64; 19]));
    assert!(b.poll().is_none());
    feed(64, 64); // duplicate retransmission must not redeliver
    assert!(b.poll().is_none());
}

#[test]
fn queued_resets_discard_old_tx_but_preserve_packets_after_latest_reset() {
    let a = Endpoint::new(VRADM_MCS_3);
    let b = Endpoint::new(VRADM_MCS_3);
    let mut reset: vradm_cmd_t = unsafe { std::mem::zeroed() };
    reset.cmd_type = VRADM_CMD_RESET_SESSION;
    a.write(&[1; 19]);
    assert_eq!(unsafe { vradm_submit_cmd(a.0, &reset) }, VRADM_OK);
    a.write(&[2; 19]);
    assert_eq!(unsafe { vradm_submit_cmd(a.0, &reset) }, VRADM_OK);
    a.write(&[3; 19]);
    let mut received = Vec::new();
    for _ in 0..600 {
        tick(&a, &b, 160, false, false);
        if let Some(packet) = b.poll() { received.push(packet); }
    }
    assert_eq!(received, vec![vec![3; 19]]);
}

#[test]
fn queued_reset_filters_rx_packets_produced_after_host_cleanup() {
    use vradm_core::arq::IpPacketSlicer;
    use vradm_core::phy::PhyTransmitter;
    let b = Endpoint::new(VRADM_MCS_3);
    // Keep an outgoing burst active so reset cannot apply immediately.
    b.write(&[0x55; 19]);
    let mut pcm = [0; 160];
    unsafe { vradm_generate_audio(b.0, pcm.as_mut_ptr(), 160) };
    let mut reset: vradm_cmd_t = unsafe { std::mem::zeroed() };
    reset.cmd_type = VRADM_CMD_RESET_SESSION;
    assert_eq!(unsafe { vradm_submit_cmd(b.0, &reset) }, VRADM_OK);
    // The capture path still belongs to the old generation until burst end.
    let mut waveform = PhyTransmitter::new(3);
    let mut frames = IpPacketSlicer::slice(&[0x44; 19], false, false, 0).unwrap();
    frames[0].ctrl = 0x3a;
    for chunk in waveform.modulate_burst(3, &frames, 1).chunks(160) {
        unsafe { vradm_process_audio(b.0, chunk.as_ptr(), chunk.len() as u32) };
    }
    // Confirm this exercised a stale packet actually published by audio.
    assert_eq!(unsafe { &*b.0 }.rx_packet_queue.len(), 1);
    assert!(b.poll().is_none());
    for _ in 0..50 { unsafe { vradm_generate_audio(b.0, pcm.as_mut_ptr(), 160) }; }
    // Fresh session data uses the same wire seq=0 but new local generation.
    for chunk in waveform.modulate_burst(3, &frames, 2).chunks(160) {
        unsafe { vradm_process_audio(b.0, chunk.as_ptr(), chunk.len() as u32) };
    }
    assert_eq!(b.poll(), Some(vec![0x44; 19]));
}

#[test]
fn rejected_reset_does_not_change_packet_generation() {
    let a = Endpoint::new(3);
    let b = Endpoint::new(3);
    let mut cmd: vradm_cmd_t = unsafe { std::mem::zeroed() };
    for _ in 0..32 { assert_eq!(unsafe { vradm_submit_cmd(a.0, &cmd) }, VRADM_OK); }
    a.write(&[0x46; 19]);
    cmd.cmd_type = VRADM_CMD_RESET_SESSION;
    assert_eq!(unsafe { vradm_submit_cmd(a.0, &cmd) }, VRADM_ERR_QUEUE_FULL);
    for _ in 0..120 { tick(&a, &b, 160, false, false); }
    assert_eq!(b.poll(), Some(vec![0x46; 19]));
}
