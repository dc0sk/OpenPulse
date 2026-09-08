//! Every KISS emission keys the transmitter (#1259).
//!
//! `openpulse-kiss` declared `openpulse-radio` in its `Cargo.toml` and never used it. It built no
//! `PttController` at all, never read `[modem] ptt_backend`, and logged nothing to say so — so an
//! operator running the APRS path with `ptt_backend = "rigctld"` played audio into an unkeyed
//! transceiver. The TX counter incremented, no warning appeared, and only VOX worked. `[modem]` is
//! captioned "Modem defaults shared by all TNC binaries".
//!
//! **There is no natural positive control here.** ARDOP's equivalent test got one for free: its
//! station-ID path keyed before the fix, so a spy that never saw an assert proved the spy was
//! mis-wired rather than the code correct. KISS deliberately runs **no** `StationIdTimer` — AX.25
//! carries the source callsign in every frame's address field, which satisfies §97.119 without a
//! separate ID cycle — so nothing here keyed before. Each test therefore keys the shared PTT
//! directly first, the way `ptt_keys_every_daemon_transmit` does.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use openpulse_radio::shared_ptt::{SharedPtt, DEFAULT_PTT_MAX};
use openpulse_radio::{PttController, PttError};

#[derive(Clone)]
struct Spy {
    asserted: Arc<AtomicBool>,
    asserts: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
}

impl Spy {
    fn new() -> Self {
        Self {
            asserted: Arc::new(AtomicBool::new(false)),
            asserts: Arc::new(AtomicUsize::new(0)),
            releases: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl PttController for Spy {
    fn assert_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(true, Ordering::SeqCst);
        self.asserts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn release_ptt(&mut self) -> Result<(), PttError> {
        self.asserted.store(false, Ordering::SeqCst);
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn is_asserted(&self) -> bool {
        self.asserted.load(Ordering::SeqCst)
    }
}

/// The bridge carries a `SharedPtt`, and it keys and releases.
///
/// The direct key is the positive control: without it, a spy reporting zero asserts would be
/// indistinguishable from a spy that was never wired to anything.
#[test]
fn the_shared_ptt_is_wired_and_keys() {
    let spy = Spy::new();
    let ptt = SharedPtt::new(Some(Box::new(spy.clone())), DEFAULT_PTT_MAX);

    assert_eq!(spy.asserts.load(Ordering::SeqCst), 0, "nothing keyed yet");
    {
        let _g = ptt.keyed(None).expect("assert");
        assert!(
            spy.asserted.load(Ordering::SeqCst),
            "keyed inside the guard"
        );
    }
    assert!(
        !spy.asserted.load(Ordering::SeqCst),
        "released when the guard dropped"
    );
    assert_eq!(spy.asserts.load(Ordering::SeqCst), 1);
    assert_eq!(spy.releases.load(Ordering::SeqCst), 1);
}

/// THE #1299 GATE: the crate's `SharedPtt` has a watchdog thread, so its deadline is enforced.
///
/// `openpulse-kiss` built a `SharedPtt` and took guards from it, but called `spawn_watchdog`
/// nowhere — so `force_release_if_expired` had no caller anywhere in the crate and the deadline was
/// never checked. A `SharedPtt` with no watchdog thread is the bare `Box` with extra steps.
///
/// The RAII guard already covers an early return and an unwind. What it cannot reach is a transmit
/// that **blocks** rather than returns, which is exactly the case a watchdog exists for — and on a
/// real rig that is a stuck carrier with nothing to take it back.
///
/// Drives the real constructor (`KissServer::with_ptt`), not a hand-built `SharedPtt`: the defect
/// was in the wiring, so a test that builds its own would pass against the unfixed crate.
#[test]
fn the_watchdog_force_releases_a_key_that_outlives_its_deadline() {
    let spy = Spy::new();
    let engine = openpulse_modem::ModemEngine::new(Box::new(
        openpulse_audio::loopback::LoopbackBackend::default(),
    ));
    let server = openpulse_kiss::KissServer::with_ptt(
        engine,
        openpulse_kiss::KissConfig {
            bind_addr: "127.0.0.1".into(),
            port: 0,
            mode: "BPSK250".into(),
            loopback: true,
        },
        Default::default(),
        None,
        Some(Box::new(spy.clone())),
    );
    let ptt = server.bridge().ptt.clone();
    // Shorten the 180 s production deadline; the watchdog ticks every 100 ms.
    ptt.set_max_duration(std::time::Duration::from_millis(120));

    // Key WITHOUT holding a guard, standing in for a transmit that blocks past the deadline: a
    // dropped guard would release on its own and prove nothing about the watchdog.
    ptt.key(None).expect("assert");
    assert!(spy.asserted.load(Ordering::SeqCst), "keyed");

    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(
        !spy.asserted.load(Ordering::SeqCst),
        "the transmitter is STILL KEYED past its deadline — the crate builds a SharedPtt but never \
         starts its watchdog, so nothing calls force_release_if_expired (#1299)"
    );
    assert_eq!(
        spy.releases.load(Ordering::SeqCst),
        1,
        "exactly one release: the watchdog is single-fire"
    );
}

/// **Source scan.** Every `.transmit(` in `bridge.rs` sits inside `keyed_transmit`.
///
/// The pattern is `.transmit(`, NOT `engine.transmit` as the ARDOP and daemon scanners use. KISS
/// writes its calls as multi-line chains where `engine` and `.transmit(` are five lines apart, so
/// the copied pattern would match **nothing** — a vacuous gate whose planted single-line control
/// still passes. That is why the control below is deliberately multi-line.
#[test]
fn every_transmit_in_the_bridge_is_keyed() {
    let src = include_str!("../src/bridge.rs");
    let bare = bare_transmit_calls(src);
    assert!(
        bare.is_empty(),
        "these `.transmit(` call sites are not inside `keyed_transmit`, so they would put audio on \
         the air without keying the rig: {bare:?}"
    );

    // Validate the scanner against a planted call in the SHAPE KISS actually uses. Without this,
    // a pattern that had rotted would report zero and read as success.
    let planted = format!(
        "{src}\nfn zz_planted() {{\n    bridge\n        .engine\n        .lock()\n        \
         .unwrap()\n        .transmit(&data, &mode, None);\n}}\n"
    );
    assert_eq!(
        bare_transmit_calls(&planted).len(),
        1,
        "the scanner must see a planted multi-line transmit — if it reports 0 here, the real \
         assertion above proves nothing"
    );
}

/// `.transmit(` call sites that are not the one inside `keyed_transmit`.
///
/// Comment lines are stripped first. A doc comment in this very file explains that relay forwarding
/// "must therefore call `engine.transmit(...)`", and the first version of this scanner counted that
/// sentence as a call site — the #1192 defect (prose read as a reference) reproduced in a scanner
/// written after #1192 was fixed elsewhere. A scan over source text has to decide what is code.
fn bare_transmit_calls(src: &str) -> Vec<usize> {
    let src: String = src
        .lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let src = src.as_str();
    let in_helper = src
        .find("fn keyed_transmit(")
        .map(|start| {
            let end = src[start..]
                .find("\n}\n")
                .map(|e| start + e)
                .unwrap_or(src.len());
            start..end
        })
        .expect("keyed_transmit must exist");

    src.match_indices(".transmit(")
        .filter(|(idx, _)| !in_helper.contains(idx))
        .map(|(idx, _)| src[..idx].matches('\n').count() + 1)
        .collect()
}
