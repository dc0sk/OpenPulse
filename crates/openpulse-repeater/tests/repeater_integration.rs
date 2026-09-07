use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use bpsk_plugin::BpskPlugin;
use openpulse_audio::LoopbackBackend;
use openpulse_modem::ModemEngine;
use openpulse_radio::NoOpPtt;
use openpulse_repeater::{CrossBandRepeater, RepeaterConfig};

fn make_engine_with_plugin() -> (ModemEngine, LoopbackBackend) {
    let lb = LoopbackBackend::new();
    let mut engine = ModemEngine::new(Box::new(lb.clone_shared()));
    engine
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register");
    (engine, lb)
}

/// Spawn a minimal mock rigctld that records PTT commands.
fn spawn_mock_rigctld_with_ptt_log(ptt_log: Arc<std::sync::Mutex<Vec<&'static str>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.expect("accept");
            let log = ptt_log.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().expect("clone");
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let cmd = match line {
                        Ok(l) => l,
                        Err(_) => break,
                    };
                    match cmd.trim() {
                        "T 1" => {
                            log.lock().unwrap().push("T 1");
                            writeln!(writer, "RPRT 0").ok();
                        }
                        "T 0" => {
                            log.lock().unwrap().push("T 0");
                            writeln!(writer, "RPRT 0").ok();
                        }
                        _ => {
                            writeln!(writer, "RPRT 0").ok();
                        }
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn relay_disabled_returns_none() {
    let (engine_rx, _lb_rx) = make_engine_with_plugin();
    let (engine_tx, _lb_tx) = make_engine_with_plugin();
    let config = RepeaterConfig {
        enabled: false,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: false,
        ..Default::default()
    };
    let mut repeater =
        CrossBandRepeater::new(Box::new(NoOpPtt::new()), engine_rx, engine_tx, config);
    let result = repeater.relay_one_frame().expect("no error");
    assert_eq!(result, None);
}

#[test]
fn relay_loopback_cross_band() {
    // Source → encode → loopback_a → engine_rx → relay → engine_tx → loopback_b → decode
    let (engine_rx, lb_rx) = make_engine_with_plugin();
    let (engine_tx, lb_tx) = make_engine_with_plugin();

    // Encode a frame into lb_rx via a separate source engine.
    let mut src_engine = ModemEngine::new(Box::new(lb_rx.clone_shared()));
    src_engine
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register src");
    let payload = b"cross-band relay test";
    src_engine
        .transmit(payload, "BPSK250", None)
        .expect("transmit");

    let ptt_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock_addr = spawn_mock_rigctld_with_ptt_log(ptt_log.clone());

    // Give the mock server a moment to start
    std::thread::sleep(std::time::Duration::from_millis(10));

    let rig_b = openpulse_radio::RigctldController::connect(&mock_addr).expect("connect");

    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: false,
        ..Default::default()
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);
    let n = repeater.relay_one_frame().expect("relay").expect("Some");
    assert_eq!(n, payload.len());

    // Verify PTT was asserted then released.
    let log = ptt_log.lock().unwrap();
    assert_eq!(*log, vec!["T 1", "T 0"]);

    // Decode what arrived in lb_tx.
    let mut sink_engine = ModemEngine::new(Box::new(lb_tx.clone_shared()));
    sink_engine
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register sink");
    let received = sink_engine.receive("BPSK250", None).expect("receive");
    assert_eq!(&received[..payload.len()], payload);
}

/// A PTT double that records each assert/release, to observe the extra keying the ID performs.
struct LoggingPtt {
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    asserted: bool,
}
impl openpulse_radio::PttController for LoggingPtt {
    fn assert_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
        self.asserted = true;
        self.log.lock().unwrap().push("assert");
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), openpulse_radio::PttError> {
        self.asserted = false;
        self.log.lock().unwrap().push("release");
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        self.asserted
    }
}

#[test]
fn transmitting_rig_is_station_identified_when_the_interval_elapses() {
    // #1260: this used to assert the PTT edge sequence `["assert","assert","release","release"]` —
    // a proxy for "an ID went out", and the proxy is what let the double-key ship as a pass. The
    // second assert was `maybe_identify` keying *underneath* the frame's still-live key, and its
    // release dropped rig_b mid-scope. Assert the ID's BYTES on the transmit side instead, and
    // require exactly ONE keying pair covering frame and ID together.
    fn feed_frame(lb: &LoopbackBackend) {
        let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
        src.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        src.transmit(b"relay frame", "BPSK250", None).expect("tx");
    }

    /// Audio length of one BPSK250 transmission of `payload`, measured rather than assumed — it is
    /// what splits the two-transmission capture below into its frame and its ID.
    fn audio_len_of(payload: &[u8]) -> usize {
        let lb = LoopbackBackend::new();
        let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        e.transmit(payload, "BPSK250", None).expect("tx");
        lb.drain_samples().len()
    }

    /// Decode one BPSK250 transmission out of `samples`.
    fn decode(samples: &[f32]) -> String {
        let lb = LoopbackBackend::new();
        let mut e = ModemEngine::new(Box::new(lb.clone_shared()));
        e.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        lb.fill_samples(samples);
        String::from_utf8_lossy(&e.receive("BPSK250", None).expect("decode")).into_owned()
    }

    let (engine_rx, lb_rx) = make_engine_with_plugin();
    let (engine_tx, lb_tx) = make_engine_with_plugin();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let rig_b = LoggingPtt {
        log: log.clone(),
        asserted: false,
    };
    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: false,
        callsign: "N0CALL".into(),
        id_interval_secs: 600,
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);

    // First relay at t=0: one keying pair for the relayed frame, no ID yet.
    feed_frame(&lb_rx);
    repeater
        .relay_one_frame_at(0)
        .expect("relay")
        .expect("Some");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["assert", "release"],
        "a plain relay keys once; no ID before the interval"
    );
    assert_eq!(
        decode(&lb_tx.drain_samples()),
        "relay frame",
        "the relayed frame must reach the transmitting rig"
    );

    // Second relay at t = 601 s: the interval has elapsed, so the ID goes out under the SAME key.
    log.lock().unwrap().clear();
    feed_frame(&lb_rx);
    repeater
        .relay_one_frame_at(601_000)
        .expect("relay")
        .expect("Some");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["assert", "release"],
        "the ID must ride the frame's key — a second assert underneath a live key releases rig_b \
         mid-scope, and would be refused outright by the #1263 rule"
    );

    let captured = lb_tx.drain_samples();
    let frame_len = audio_len_of(b"relay frame");
    assert!(
        captured.len() > frame_len,
        "only one transmission reached rig_b, so no ID was sent (captured {} samples, one frame is {})",
        captured.len(),
        frame_len
    );
    assert_eq!(
        decode(&captured[..frame_len]),
        "relay frame",
        "the traffic must precede the ID that identifies it"
    );
    assert_eq!(
        decode(&captured[frame_len..]),
        "DE N0CALL",
        "§97.119: the transmitting rig must actually send its callsign, not merely key twice"
    );
}

#[test]
fn relay_empty_buffer_returns_none() {
    let (engine_rx, _lb_rx) = make_engine_with_plugin();
    let (engine_tx, lb_tx) = make_engine_with_plugin();
    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: false,
        ..Default::default()
    };
    // No samples in loopback_rx. Whether receive() reports empty or errors is an implementation
    // detail, but the contract that matters is the same either way: nothing may be relayed, and
    // nothing may be transmitted. Accepting "any outcome" (as this test used to) would also accept
    // a repeater that keyed up and relayed garbage on an empty buffer.
    let mut repeater =
        CrossBandRepeater::new(Box::new(NoOpPtt::new()), engine_rx, engine_tx, config);
    match repeater.relay_one_frame() {
        Ok(None) => {}
        Ok(Some(n)) => panic!("relayed {n} bytes from an empty receive buffer"),
        Err(_) => {} // receive() surfacing an error on an empty buffer is acceptable
    }
    assert!(
        lb_tx.drain_samples().is_empty(),
        "nothing may be transmitted when there was nothing to relay"
    );
}

#[test]
fn full_duplex_idle_session_never_keys_and_a_held_key_is_released_at_session_end() {
    // #1260 behaviour change: full duplex no longer keys eagerly at session start. The watchdog is
    // in-process, so an unbounded deliberate hold does not mean "a hung repeater keys forever" — it
    // means a *dead daemon* leaves rig_b keyed, since nothing releases PTT on shutdown and
    // `RigctldPtt` has no `Drop`. Keying eagerly made that the state of an IDLE repeater.
    let (engine_rx, _lb_rx) = make_engine_with_plugin();
    let (engine_tx, _lb_tx) = make_engine_with_plugin();

    let ptt_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock_addr = spawn_mock_rigctld_with_ptt_log(ptt_log.clone());
    std::thread::sleep(std::time::Duration::from_millis(10));
    let rig_b = openpulse_radio::RigctldController::connect(&mock_addr).expect("connect");

    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 500, // ignored in full-duplex
        full_duplex: true,
        ..Default::default()
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);

    let stop = Arc::new(AtomicBool::new(true)); // already stopped
    let count = repeater.run_full_duplex(stop).expect("no error");
    assert_eq!(count, 0);

    assert!(
        ptt_log.lock().unwrap().is_empty(),
        "a full-duplex session that relayed nothing must not have keyed the transmitter"
    );
}

#[test]
fn full_duplex_holds_one_key_across_frames_and_releases_it_at_session_end() {
    let (engine_rx, lb_rx) = make_engine_with_plugin();
    let (engine_tx, _lb_tx) = make_engine_with_plugin();

    let feed = |lb: &LoopbackBackend| {
        let mut src = ModemEngine::new(Box::new(lb.clone_shared()));
        src.register_plugin(Box::new(BpskPlugin::new()))
            .expect("register");
        src.transmit(b"fd frame", "BPSK250", None).expect("tx");
    };

    let ptt_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock_addr = spawn_mock_rigctld_with_ptt_log(ptt_log.clone());
    std::thread::sleep(std::time::Duration::from_millis(10));
    let rig_b = openpulse_radio::RigctldController::connect(&mock_addr).expect("connect");

    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: true,
        ..Default::default()
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);

    for t in [0u64, 1_000] {
        feed(&lb_rx);
        repeater
            .relay_one_frame_at(t)
            .expect("relay")
            .expect("Some");
    }
    assert_eq!(
        *ptt_log.lock().unwrap(),
        vec!["T 1"],
        "full duplex must key ONCE and hold the key across frames — that is what the flag buys"
    );

    // Session end releases whatever is still held, on the stop path as well as the error path.
    repeater
        .run_full_duplex(Arc::new(AtomicBool::new(true)))
        .expect("no error");
    assert_eq!(
        *ptt_log.lock().unwrap(),
        vec!["T 1", "T 0"],
        "the held key must be released when the session ends"
    );
}

#[test]
fn full_duplex_disabled_returns_zero_immediately() {
    // enabled=false → run_full_duplex returns Ok(0) without touching PTT.
    let (engine_rx, _lb_rx) = make_engine_with_plugin();
    let (engine_tx, _lb_tx) = make_engine_with_plugin();

    let ptt_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock_addr = spawn_mock_rigctld_with_ptt_log(ptt_log.clone());
    std::thread::sleep(std::time::Duration::from_millis(10));
    let rig_b = openpulse_radio::RigctldController::connect(&mock_addr).expect("connect");

    let config = RepeaterConfig {
        enabled: false,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: true,
        ..Default::default()
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);

    let stop = Arc::new(AtomicBool::new(false));
    let count = repeater.run_full_duplex(stop).expect("no error");
    assert_eq!(count, 0);

    let log = ptt_log.lock().unwrap();
    assert!(log.is_empty(), "PTT must not be touched when disabled");
}

#[test]
fn full_duplex_relay_one_frame_keys_rather_than_transmitting_into_an_unkeyed_rig() {
    // Was `full_duplex_relay_one_frame_skips_ptt`, which asserted an EMPTY PTT log for a call that
    // transmits. That was correct only because `run_full_duplex` keyed eagerly first; called on its
    // own — as it is public and as the test itself calls it — it played audio into an unkeyed rig.
    let (engine_rx, lb_rx) = make_engine_with_plugin();
    let (engine_tx, _lb_tx) = make_engine_with_plugin();

    let mut src_engine = ModemEngine::new(Box::new(lb_rx.clone_shared()));
    src_engine
        .register_plugin(Box::new(BpskPlugin::new()))
        .expect("register src");
    src_engine
        .transmit(b"fd frame", "BPSK250", None)
        .expect("transmit");

    let ptt_log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mock_addr = spawn_mock_rigctld_with_ptt_log(ptt_log.clone());
    std::thread::sleep(std::time::Duration::from_millis(10));
    let rig_b = openpulse_radio::RigctldController::connect(&mock_addr).expect("connect");

    let config = RepeaterConfig {
        enabled: true,
        mode: "BPSK250".into(),
        tx_hang_ms: 0,
        full_duplex: true,
        ..Default::default()
    };
    let mut repeater = CrossBandRepeater::new(Box::new(rig_b), engine_rx, engine_tx, config);

    let result = repeater.relay_one_frame().expect("relay");
    assert!(result.is_some(), "expected a frame to relay");
    assert_eq!(
        *ptt_log.lock().unwrap(),
        vec!["T 1"],
        "a full-duplex relay must key before transmitting, and hold rather than release"
    );
}
