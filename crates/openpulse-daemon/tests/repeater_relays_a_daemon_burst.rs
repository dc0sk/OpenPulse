//! THE #1308 GATE: a burst the daemon flushes must reach the cross-band repeater and be relayed.
//!
//! **Why this test did not exist.** The repeater used to open a capture stream of its own, from a
//! backend that `build_audio_backend` hands it *fresh*. Through `server::run` it therefore listened
//! to a `LoopbackBackend` nobody wrote to: it heard nothing this daemon heard, in production and in
//! every test. Every repeater gate in the tree drives `CrossBandRepeater` directly — none goes
//! through the daemon — so the crate's own suite could be entirely green while the shipped daemon
//! relayed nothing. #1308's decision is one accumulator, one flush, two consumers (the monitor and
//! the repeater), which is what this asserts.
//!
//! **The observable is the transmitter**, not a count or an event: a mock rigctld on `[radio.rig_b]`
//! records `T 1`. Nothing else in the daemon keys rig_b — it is the repeater's rig alone — and the
//! §97.119 ID cannot key it first, because `StationIdTimer::id_due` requires a prior transmit. So a
//! `T 1` on rig_b means a frame was relayed.

use std::io::{BufRead, BufReader as IoBufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openpulse_config::OpenpulseConfig;
use openpulse_core::audio::{
    AudioBackend, AudioConfig, AudioInputStream, AudioOutputStream, DeviceInfo,
};
use openpulse_core::error::AudioError;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const MODE: &str = "BPSK250";

// ── A mock rigctld that counts PTT keying on rig_b ────────────────────────────

/// Spawn a mock rigctld, returning its address and the count of `T 1` (assert-PTT) commands.
fn spawn_mock_rigctld() -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock rigctld");
    let addr = listener.local_addr().expect("addr").to_string();
    let keys = Arc::new(AtomicU64::new(0));
    let keys_ret = Arc::clone(&keys);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let keys = Arc::clone(&keys);
            std::thread::spawn(move || {
                let mut writer = stream.try_clone().expect("clone");
                for line in IoBufReader::new(stream).lines().map_while(Result::ok) {
                    match line.trim() {
                        "T 1" => {
                            keys.fetch_add(1, Ordering::SeqCst);
                            writeln!(writer, "RPRT 0").ok();
                        }
                        "T 0" => {
                            writeln!(writer, "RPRT 0").ok();
                        }
                        "t" => {
                            writeln!(writer, "0").ok();
                        }
                        _ => {
                            writeln!(writer, "RPRT 0").ok();
                        }
                    }
                }
            });
        }
    });
    (addr, keys_ret)
}

// ── A backend that replays one prepared burst, then silence, then re-arms ─────

/// Feeds the daemon a real frame followed by silence, so the receive tick sees a carrier and then a
/// carrier drop and flushes exactly one burst — the shape a real capture has.
///
/// The burst RECURS. A one-shot backend would make this test a race against the control client's
/// connect and the enable command, which is the trap `monitor_during_ota` was merged red for.
#[derive(Clone)]
struct ReplayBackend {
    pending: Arc<Mutex<Vec<f32>>>,
    frame: Vec<f32>,
}

struct ReplayStream {
    pending: Arc<Mutex<Vec<f32>>>,
    frame: Vec<f32>,
    silence_reads: usize,
}

/// Enough silence for `accumulate_capture` to see the carrier drop and flush, plus margin.
const SILENCE_READS_BETWEEN_BURSTS: usize = 4;

impl AudioInputStream for ReplayStream {
    fn read(&mut self) -> Result<Vec<f32>, AudioError> {
        let mut g = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            self.silence_reads += 1;
            if self.silence_reads >= SILENCE_READS_BETWEEN_BURSTS {
                self.silence_reads = 0;
                *g = self.frame.clone();
            }
            Ok(vec![0.0; 800])
        } else {
            let take = g.len().min(4096);
            Ok(g.drain(..take).collect())
        }
    }
    fn close(self: Box<Self>) {}
}

struct NullOut;
impl AudioOutputStream for NullOut {
    fn write(&mut self, _samples: &[f32]) -> Result<(), AudioError> {
        Ok(())
    }
    fn flush(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
    fn close(self: Box<Self>) {}
}

impl AudioBackend for ReplayBackend {
    fn name(&self) -> &str {
        "Replay"
    }
    fn list_devices(&self) -> Result<Vec<DeviceInfo>, AudioError> {
        Ok(vec![])
    }
    fn open_input(
        &self,
        _d: Option<&str>,
        _c: &AudioConfig,
    ) -> Result<Box<dyn AudioInputStream>, AudioError> {
        Ok(Box::new(ReplayStream {
            pending: Arc::clone(&self.pending),
            frame: self.frame.clone(),
            silence_reads: 0,
        }))
    }
    fn open_output(
        &self,
        _d: Option<&str>,
        _c: &AudioConfig,
    ) -> Result<Box<dyn AudioOutputStream>, AudioError> {
        Ok(Box::new(NullOut))
    }
}

/// Modulate one real frame in `MODE`, so the replayed audio is a decodable burst, not synthetic noise.
fn one_frame() -> Vec<f32> {
    let lb = openpulse_audio::LoopbackBackend::new();
    let mut e = openpulse_modem::ModemEngine::new(Box::new(lb.clone_shared()));
    e.register_plugin(Box::new(bpsk_plugin::BpskPlugin::new()))
        .expect("register bpsk");
    e.transmit(b"relay me", MODE, None).expect("transmit");
    let mut samples = lb.drain_samples();
    assert!(!samples.is_empty(), "fixture frame is empty");
    // Lead-in silence so the energy gate sees a rising edge rather than starting mid-carrier.
    let mut out = vec![0.0f32; 1600];
    out.append(&mut samples);
    out
}

fn cfg(tcp_port: u16, ws_port: u16, rig_b_addr: &str, autostart: bool) -> OpenpulseConfig {
    let mut c = OpenpulseConfig::default();
    c.station.callsign = "TESTER".into();
    c.modem.mode = MODE.into();
    c.daemon.tcp_port = tcp_port;
    c.daemon.websocket_port = ws_port;
    // The MAIN rig must not reach a rigctld at all, or #1260's alias rule and the meter poll both
    // come into play for reasons that have nothing to do with this gate.
    c.modem.ptt_backend = "none".into();
    c.radio.cat_backend = "none".into();
    c.repeater.enabled = autostart;
    c.repeater.mode = MODE.into();
    c.repeater.full_duplex = false;
    c.radio.rig_b = Some(openpulse_config::RigConfig {
        rigctld_addr: rig_b_addr.to_string(),
        backend: "rigctld".into(),
        serial_port: String::new(),
        rig_file: String::new(),
    });
    c
}

fn spawn_daemon(cfg: OpenpulseConfig, backend: ReplayBackend) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("daemon runtime");
        rt.block_on(async move {
            if let Err(e) = openpulse_daemon::server::run(cfg, Box::new(backend)).await {
                eprintln!("daemon refused to start: {e}");
            }
        });
    });
}

async fn send_cmd(sock: &mut tokio::net::tcp::OwnedWriteHalf, json: &str) {
    sock.write_all(json.as_bytes()).await.expect("write cmd");
    sock.write_all(b"\n").await.expect("write nl");
    sock.flush().await.expect("flush");
}

/// Spin up a daemon on its own ports and return the rig_b key counter.
fn daemon(tcp: u16, ws: u16, autostart: bool) -> Arc<AtomicU64> {
    let (rig_b_addr, keys) = spawn_mock_rigctld();
    let frame = one_frame();
    spawn_daemon(
        cfg(tcp, ws, &rig_b_addr, autostart),
        ReplayBackend {
            pending: Arc::new(Mutex::new(frame.clone())),
            frame,
        },
    );
    keys
}

async fn keyed_within(keys: &Arc<AtomicU64>, secs: u64) -> bool {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if keys.load(Ordering::SeqCst) > 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// Read control events until one names the repeater, so a failure says WHICH answer came back.
async fn repeater_answer(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return "control stream closed".to_string();
            }
            let t = line.trim();
            if t.contains("repeater_changed") || t.contains("command_error") {
                return t.to_string();
            }
        }
    })
    .await
    .unwrap_or_else(|_| "no answer within 5 s".to_string())
}

/// THE GATE (a): `[repeater] enabled = true` relays with NO command at all.
///
/// This is the case that has never worked in the project's history. Startup set
/// `repeater_enabled = true` from config while spawning no thread, so the station reported a running
/// repeater, `EnableRepeater` answered "already enabled", and nothing relayed until an operator
/// happened to send `DisableRepeater` first — a sequence the panel cannot even express.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_config_enabled_repeater_relays_with_no_command() {
    let keys = daemon(19180, 19181, true);
    assert!(
        keyed_within(&keys, 25).await,
        "rig_b was never keyed within 25 s on a daemon configured `[repeater] enabled = true`. \
         No control command was sent, and none should be needed: config means running, as it does \
         for the JS8 discovery beacon. Check that `start_repeater_if_configured` runs at startup \
         and that the rx tick try_sends the flushed burst to `runtime_state.repeater_bursts`."
    );
}

/// THE GATE (b): `enable_repeater` on a config-disabled daemon actually starts it.
///
/// This used to emit `RepeaterChanged { enabled: true }` and relay nothing, because the spawned
/// thread returned `Ok(0)` at once on `RepeaterConfig.enabled` — #1298's class surviving inside the
/// arm #1298 hardened. The field is gone; the daemon owns the lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enable_repeater_starts_a_config_disabled_repeater() {
    let keys = daemon(19182, 19183, false);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sock = TcpStream::connect("127.0.0.1:19182")
        .await
        .expect("control port");
    let (r, mut w) = sock.into_split();
    let mut reader = BufReader::new(r);
    send_cmd(&mut w, r#"{"cmd":"enable_repeater"}"#).await;
    let answer = repeater_answer(&mut reader).await;

    assert!(
        answer.contains(r#""enabled":true"#),
        "enable_repeater did not report the repeater started: {answer}"
    );
    assert!(
        keyed_within(&keys, 25).await,
        "enable_repeater reported success but rig_b was never keyed: the daemon announced a \
         repeater it did not run. Answer was: {answer}"
    );
}

/// THE GATE (c): when the daemon says "already enabled", that must be TRUE.
///
/// The refusal itself is correct — what was wrong is that it used to be a lie on a config-enabled
/// daemon, where nothing was running to be already-enabled. Asserting the refusal alone would pass
/// against the old broken build, so this also requires the station to be relaying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn already_enabled_is_refused_only_when_it_is_true() {
    let keys = daemon(19184, 19185, true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sock = TcpStream::connect("127.0.0.1:19184")
        .await
        .expect("control port");
    let (r, mut w) = sock.into_split();
    let mut reader = BufReader::new(r);
    send_cmd(&mut w, r#"{"cmd":"enable_repeater"}"#).await;
    let answer = repeater_answer(&mut reader).await;

    assert!(
        answer.contains("already enabled"),
        "a second enable_repeater should be refused, got: {answer}"
    );
    assert!(
        keyed_within(&keys, 25).await,
        "the daemon refused enable_repeater as `already enabled` while relaying nothing — the \
         refusal was a lie. Answer was: {answer}"
    );
}
