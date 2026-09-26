use vradm_core::{c_abi::*, engine::vradm_engine};
fn config(amplitude: f32) -> vradm_config_t {
    vradm_config_t {
        sample_rate: 8000,
        startup_mcs: 2,
        auto_rate_adaptation: 0,
        reserved: [0; 2],
        tx_amplitude: amplitude,
        reserved2: [0; 4],
        psk_key: [1; 16],
    }
}
fn command(amplitude: f32) -> vradm_cmd_t {
    vradm_cmd_t {
        cmd_type: VRADM_CMD_SET_TX_PARAMS,
        cmd_id: 0,
        param_u32: 0,
        param_i32: 0,
        param_f32: amplitude,
        inline_payload: [0; 12],
    }
}
fn burst(amplitude: f32) -> Vec<i16> {
    let mut engine = vradm_engine::new(config(amplitude));
    let (mut host, mut audio) = engine.split();
    assert_eq!(host.write_ip_packet(&[42; 19]), VRADM_OK);
    let mut out = vec![0; 9720]; // nominal PLCP + one MCS2 frame
    audio.generate_audio(&mut out);
    out
}
fn rms(pcm: &[i16]) -> f64 {
    (pcm.iter()
        .map(|&v| (v as f64 / 32767.0).powi(2))
        .sum::<f64>()
        / pcm.len() as f64)
        .sqrt()
}
#[test]
fn configured_ceiling_controls_live_burst_and_zero_mutes() {
    assert!(burst(0.0).iter().all(|&v| v == 0));
    let low = burst(0.02);
    let high = burst(0.04);
    assert!((rms(&low) - 0.02).abs() < 0.0001);
    assert!(rms(&high) > 0.038 && rms(&high) <= 0.0401);
    assert_eq!(burst(1.0), burst(0.3535)); // mode and peak limits still cap output
    assert!(burst(1.0).iter().all(|v| v.unsigned_abs() <= 16384));
}
#[test]
fn command_does_not_change_buffered_burst_and_applies_to_next_burst() {
    let expected = burst(0.04);
    let mut engine = vradm_engine::new(config(0.04));
    let (mut host, mut audio) = engine.split();
    host.write_ip_packet(&[42; 19]);
    let mut out = vec![0; 9720];
    audio.generate_audio(&mut out[..160]);
    assert_eq!(host.submit_cmd(&command(0.0)), VRADM_OK);
    audio.generate_audio(&mut out[160..]);
    assert_eq!(out, expected);
    // The command is serviced before the next retry/turn is synthesized.
    let mut chunk = [123; 160];
    for _ in 0..500 {
        audio.generate_audio(&mut chunk);
        assert_eq!(chunk, [0; 160]);
    }
}
#[test]
fn invalid_commands_do_not_consume_queue_capacity_or_change_output() {
    let mut engine = vradm_engine::new(config(0.04));
    let (mut host, mut audio) = engine.split();
    for _ in 0..40 {
        for value in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            assert_eq!(host.submit_cmd(&command(value)), VRADM_ERR_INVALID_ARG);
        }
    }
    assert_eq!(host.submit_cmd(&command(0.02)), VRADM_OK);
    host.write_ip_packet(&[42; 19]);
    let mut out = vec![0; 9720];
    audio.generate_audio(&mut out);
    assert_eq!(out, burst(0.02));
}
#[test]
fn invalid_amplitudes_are_rejected_by_c_and_authenticated_constructors() {
    for amplitude in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 1.1] {
        assert!(unsafe { vradm_create(&config(amplitude)) }.is_null());
        assert!(matches!(
            vradm_engine::new_authenticated(config(amplitude)),
            Err(VRADM_ERR_INVALID_ARG)
        ));
    }
    assert!(burst(f32::NAN).iter().all(|&v| v == 0));
}
#[test]
fn c_abi_configuration_reaches_pcm_generation() {
    unsafe {
        let engine = vradm_create(&config(0.02));
        assert!(!engine.is_null());
        let packet = [42u8; 19];
        assert_eq!(
            vradm_write_ip_packet(engine, packet.as_ptr(), packet.len() as u32),
            VRADM_OK
        );
        let mut pcm = vec![0; 9720];
        vradm_generate_audio(engine, pcm.as_mut_ptr(), pcm.len() as u32);
        vradm_destroy(engine);
        assert_eq!(pcm, burst(0.02));
    }
}

#[test]
fn session_reset_preserves_applied_amplitude_setting() {
    let mut engine = vradm_engine::new(config(0.04));
    {
        let (mut host, mut audio) = engine.split();
        host.submit_cmd(&command(0.02));
        audio.generate_audio(&mut [0; 160]);
    }
    engine.reset_exclusive();
    let (mut host, mut audio) = engine.split();
    host.write_ip_packet(&[42; 19]);
    let mut out = vec![0; 9720];
    audio.generate_audio(&mut out);
    assert_eq!(out, burst(0.02));
}
