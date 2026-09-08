//! Every front-end binary that builds a `ModemEngine` pins `[audio] device` (#1311).
//!
//! **Why a source scan.** The engine resolves a device as `device.or(self.default_device)`
//! (`stage_capture_input`), so a front-end can name its device either per call or once at
//! construction. Two shipped surfaces did it correctly by two DIFFERENT mechanisms — the daemon via
//! `set_default_device`, the CLI by threading `--device` into every call — and three did neither,
//! passing a hardcoded `None` and silently taking the OS default. On a host with a USB soundcard
//! interface plus onboard audio, that is the wrong card, with no diagnostic.
//!
//! Nothing about the type system makes the omission visible, and there are seven copies of the
//! backend-selection `match` across the workspace, so there is no shared place where pinning the
//! device would have been the obvious next line. A scan is the cheapest thing that fails when the
//! next front-end forgets — the same instrument `ptt_keys_every_transmit` uses for keying.

/// Front-ends that construct an engine and must pin the configured device.
///
/// NOT listed, deliberately:
/// * `openpulse-cli` — correct by the other mechanism (threads `--device` per call).
/// * `openpulse-daemon` — its main engine pins correctly; its two REPEATER engines do not, which is
///   #1308 and is blocked on a maintainer ruling about which device they should even use. Adding it
///   here would encode an answer that has not been decided.
/// * `openpulse-mesh` — its real-audio capability was removed and `no_real_audio.rs` keeps it out.
const FRONT_ENDS: &[(&str, &str)] = &[
    (
        "openpulse-ardop",
        include_str!("../../openpulse-ardop/src/main.rs"),
    ),
    (
        "openpulse-kiss",
        include_str!("../../openpulse-kiss/src/main.rs"),
    ),
    (
        "openpulse-tui",
        include_str!("../../openpulse-tui/src/main.rs"),
    ),
];

/// Lines that construct an engine without pinning a device within `WINDOW` lines after it.
const WINDOW: usize = 12;

fn unpinned_constructions(src: &str) -> Vec<usize> {
    let lines: Vec<&str> = src.lines().collect();
    let mut bad = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !l.contains("ModemEngine::new(") {
            continue;
        }
        let end = (i + WINDOW).min(lines.len());
        if !lines[i..end]
            .iter()
            .any(|w| w.contains("set_default_device"))
        {
            bad.push(i + 1);
        }
    }
    bad
}

#[test]
fn every_front_end_pins_the_configured_audio_device() {
    for (name, src) in FRONT_ENDS {
        let bad = unpinned_constructions(src);
        assert!(
            bad.is_empty(),
            "{name} builds a ModemEngine at line(s) {bad:?} without calling set_default_device \
             within {WINDOW} lines. Every call site in these binaries passes `None` for the \
             per-call device, so without the default the engine takes the OS default card and \
             `[audio] device` is silently ignored (#1311)."
        );
    }
}

/// THE SCANNER'S OWN VALIDATION — both directions, so it cannot go vacuous.
///
/// A scan that matches nothing passes exactly like a scan over compliant code. `ptt_keys_every_transmit`
/// learned this the hard way: a pattern copied between crates matched **nothing** because the calls
/// were written as multi-line chains, and its planted control still passed.
#[test]
fn the_scanner_detects_a_planted_violation_and_accepts_a_planted_fix() {
    let violation = "\
fn main() {
    let audio = build();
    let mut engine = ModemEngine::new(audio);
    engine.register_plugin(a)?;
    engine.register_plugin(b)?;
}";
    assert_eq!(
        unpinned_constructions(violation),
        vec![3],
        "the scanner did not flag an engine built with no device pinned — it would pass over any \
         regression, which is worse than having no scan"
    );

    let fixed = "\
fn main() {
    let audio = build();
    let mut engine = ModemEngine::new(audio);
    if !cfg.audio.device.is_empty() {
        engine.set_default_device(Some(cfg.audio.device.clone()));
    }
}";
    assert!(
        unpinned_constructions(fixed).is_empty(),
        "the scanner flagged compliant code, so it would have to be silenced rather than obeyed"
    );

    // And it must not be satisfied by a mention that is too far away to be the same construction.
    let far = format!(
        "let mut engine = ModemEngine::new(audio);\n{}engine.set_default_device(None);\n",
        "    // filler\n".repeat(WINDOW + 2)
    );
    assert_eq!(
        unpinned_constructions(&far),
        vec![1],
        "the scanner accepted a set_default_device far outside the construction's window"
    );
}
