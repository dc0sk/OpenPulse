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
/// * `openpulse-mesh` — its real-audio capability was removed and `no_real_audio.rs` keeps it out.
///
/// `openpulse-daemon` IS scanned since #1308 PR 3. It was excluded while its two repeater engines
/// passed nothing and the ruling on which device they should use was open; `[repeater] tx_device`
/// settled that, so the exclusion's stated reason is spent. It is the sharpest entry in the list: a
/// cross-band repeater is by definition a two-card station, so the OS default is very likely the
/// MAIN rig — i.e. the repeater keying and transmitting into the wrong radio.
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
    (
        "openpulse-daemon/server.rs",
        include_str!("../../openpulse-daemon/src/server.rs"),
    ),
];

/// How far after a construction to look for that engine's own pin.
///
/// **A line window alone is not enough, and this was measured.** The window was briefly widened to
/// 24 to accommodate the daemon's main engine (built at `server.rs:91`, pinned at `:112`), with the
/// note that a wider window could only be fooled by a pin belonging to a *different* nearby engine
/// "which no front-end currently has". The daemon has exactly that: its two repeater engines are
/// built 15 lines apart, so deleting `rx.set_default_device` still PASSED — the scan found `tx`'s.
/// Sabotage caught it immediately. The scan therefore matches the pin to the constructed
/// BINDING (`let mut rx = …` must be followed by `rx.set_default_device`), and the window is only a
/// bound on how far to search.
const WINDOW: usize = 24;

/// The binding a `ModemEngine::new` line assigns to, e.g. `rx` for `let mut rx = ModemEngine::new(`.
///
/// `None` when the construction is not a simple `let` binding (passed straight to a function, say),
/// in which case the scan falls back to accepting any pin in the window rather than inventing a
/// rule it cannot check.
fn bound_name(line: &str) -> Option<&str> {
    let after_let = line.trim_start().strip_prefix("let ")?;
    let after_mut = after_let.strip_prefix("mut ").unwrap_or(after_let);
    let name = after_mut.split('=').next()?.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some(name)
}

/// Blank out `#[cfg(test)]` modules, preserving line numbers so reported lines stay usable.
///
/// Without this the daemon is unscannable: `server.rs` has ten interleaved test modules whose
/// fixtures build engines with no device on purpose, and every one reads as a violation. Blanking
/// rather than deleting keeps the reported line numbers pointing at the real file.
fn strip_test_modules(src: &str) -> String {
    let mut out: Vec<String> = src.lines().map(|l| l.to_string()).collect();
    let mut i = 0;
    while i < out.len() {
        if out[i].trim_start().starts_with("#[cfg(test)]") {
            // Find the opening brace of the module, then brace-match to its close.
            let mut j = i;
            while j < out.len() && !out[j].contains('{') {
                j += 1;
            }
            if j >= out.len() {
                break;
            }
            let mut depth = 0i32;
            let mut k = j;
            loop {
                for c in out[k].chars() {
                    match c {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if depth <= 0 || k + 1 >= out.len() {
                    break;
                }
                k += 1;
            }
            for line in out.iter_mut().take(k + 1).skip(i) {
                line.clear();
            }
            i = k + 1;
        } else {
            i += 1;
        }
    }
    out.join("\n")
}

fn unpinned_constructions(src: &str) -> Vec<usize> {
    let stripped = strip_test_modules(src);
    let lines: Vec<&str> = stripped.lines().collect();
    let mut bad = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if !l.contains("ModemEngine::new(") {
            continue;
        }
        let end = (i + WINDOW).min(lines.len());
        let pinned = match bound_name(l) {
            // Require THIS engine's pin, not merely a pin nearby.
            Some(name) => {
                let needle = format!("{name}.set_default_device");
                lines[i..end].iter().any(|w| w.contains(&needle))
            }
            None => lines[i..end]
                .iter()
                .any(|w| w.contains("set_default_device")),
        };
        if !pinned {
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

    // A violation INSIDE a test module must be ignored, and one outside it must still be caught —
    // otherwise stripping could silently blank the whole file and the scan would pass vacuously.
    let with_test_mod = "\
fn main() {
    let mut engine = ModemEngine::new(audio);
}
#[cfg(test)]
mod tests {
    fn fixture() {
        let e = ModemEngine::new(LoopbackBackend::new());
    }
}";
    assert_eq!(
        unpinned_constructions(with_test_mod),
        vec![2],
        "stripping must ignore the fixture inside #[cfg(test)] and still flag the production \
         construction — if this returns [] the stripper ate the whole file and the scan is vacuous"
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
