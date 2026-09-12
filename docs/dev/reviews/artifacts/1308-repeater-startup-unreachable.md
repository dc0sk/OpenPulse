---
project: openpulsehf
doc: docs/dev/reviews/artifacts/1308-repeater-startup-unreachable.md
status: review
last_updated: 2026-09-09
---

# #1308 PR2 — the repeater cannot be started through `server::run` at all

## Context

PR2 of #1308 moves the repeater off its own capture and onto the daemon's flushed bursts. Writing
the *first* daemon-level relay test (none has ever existed) surfaced that there is no reachable
path through `server::run` that makes the repeater relay a frame. This predates #1308.

## The three facts

1. `run_full_duplex` has exactly ONE caller in the workspace: the `EnableRepeater` arm in
   `crates/openpulse-daemon/src/lib.rs:2656`. `server::run` never spawns the thread.
   (`grep -n 'run_full_duplex' crates/openpulse-daemon/src/server.rs` → one hit, and it is my new
   `repeater_stop.is_some()` burst gate, not a spawn.)

2. With `[repeater] enabled = true`: `server.rs:671` sets `runtime_state.repeater_enabled = true`
   while `repeater_thread` / `repeater_stop` stay `None`. `reap_finished_repeater` (lib.rs:1645)
   early-returns because `repeater_thread.as_ref().is_some_and(is_finished)` is false for `None`,
   so it never corrects the flag. `EnableRepeater` then hits `if runtime_state.repeater_enabled`
   and returns `CommandError { reason: "repeater already enabled" }`. Nothing runs.

3. With `[repeater] enabled = false`: the repeater IS built (construction is gated on `[radio.rig_b]`
   being present and usable, not on `repeater.enabled`), but `rep_cfg.enabled` is copied from
   `cfg.repeater.enabled`, so the spawned thread hits `run_full_duplex`'s
   `if !self.config.enabled { return Ok(0) }` (repeater/src/lib.rs:228) and exits immediately.

The only working sequence is `DisableRepeater` (to clear the startup flag) then `EnableRepeater`.

## What I propose, and the question

Minimum to make PR2's gate meaningful — one of:

- **A. Startup spawns it.** When `cfg.repeater.enabled` and a repeater was built, `server::run`
  starts the thread itself and populates `repeater_stop` / `repeater_thread`, exactly as the
  command arm does. Config `enabled = true` then means running.
- **B. Startup does not claim it.** `server::run` sets `repeater_enabled = false` regardless of
  config, and `RepeaterConfig.enabled` stops gating `run_full_duplex` (the runtime command becomes
  the only switch). Config `enabled = true` becomes "build it, ready to enable".

I lean **A**: it is what `[repeater] enabled = true` plainly means to an operator, and #1298's rule
("a repeater that is not running is not reported as running") is already adopted here — B leaves an
operator who wrote `enabled = true` with a station that silently relays nothing. But A puts an
automatic transmitting service on at startup, which cuts against the repo's "outward/automatic
actions off by default" convention, so I do not want to pick this unreviewed.

Also unclear: whether `RepeaterConfig.enabled` should exist at all once the daemon owns the
lifecycle, or whether removing that field is a separate change.

## Consumer

`crates/openpulse-daemon/src/lib.rs:2656` (`EnableRepeater` arm) — the sole production caller of
`run_full_duplex`. Confirmed by `grep -rn 'run_full_duplex' --include=*.rs` across the workspace:
that arm plus tests in `openpulse-repeater`. `server::run` calls it nowhere.

## Prior art

- **#1298** already fixed the "reports enabled while nothing runs" shape — but on the *command*
  path only (`reap_finished_repeater`, the no-repeater `CommandError`). The startup boundary is the
  untouched twin. This is that issue's class, one entry point over.
- **#1260** gates repeater *construction* on `[radio.rig_b]` and refuses to start on an aliased
  rigctld — so a "refuse to start" precedent for repeater misconfiguration already exists at
  startup, which is evidence for A being in keeping.
- `grep -rn 'enabled' crates/openpulse-daemon/src/server.rs` for other services: the monitor
  (`cfg.monitor.enabled`) is constructed AND runs from the rx tick with no command needed — i.e.
  config-enabled-means-running is already the daemon's convention for a receive-side service. The
  repeater differs in that it transmits.

## Twins

- **ARDOP / KISS front-ends**: neither builds a `CrossBandRepeater` (`grep -rn 'CrossBandRepeater'`
  → daemon + repeater crate only), so there is no sibling front-end with this gap.
- **`openpulse-mesh`**: had a comparable auto-transmit-at-startup capability; it was REMOVED rather
  than guarded (no PTT, no carrier sense, no ID). That is the counterweight to A — the repo has
  once chosen deletion over defaulting-on for an automatic transmitting service. The difference is
  that the repeater has rig_b PTT, a `SharedPtt` watchdog and a §97.119 ID timer. (I originally
  wrote "carrier sense" here too. That was false — see the review outcome below.)
- **`DisableRepeater`**: symmetric hole — it succeeds and emits `RepeaterChanged { enabled: false }`
  on a startup-enabled daemon that was never running. Whichever option is chosen must leave both
  commands truthful from a cold start.

## Measured, not read (added after the note was first sent for review)

The first daemon-level relay gate now exists —
`crates/openpulse-daemon/tests/repeater_relays_a_daemon_burst.rs`: a real BPSK250 frame replayed as
a recurring burst into `server::run`, with a mock rigctld on `[radio.rig_b]` counting `T 1`. It
captures the daemon's own answer to `enable_repeater`, so a red result cannot be attributed to the
wrong cause.

| Config | Daemon's answer to `enable_repeater` | rig_b keyed? |
|---|---|---|
| `[repeater] enabled = true` | `command_error … "repeater already enabled"` | no, 25 s |
| `[repeater] enabled = false` | `repeater_changed { enabled: true }` | no, 25 s |
| `enabled = true`, `disable` then `enable` | `repeater_changed { enabled: true }` | **yes, 1.9 s** |

Two things this settles:

1. **The #1308 burst handoff itself is correct and complete.** Once the repeater thread is genuinely
   running, the daemon's flushed burst reaches it and keys rig_b in 1.9 s, through `server::run`.
   Sabotage-verified: forcing the `try_send` branch to fail turns that 1.9 s pass into a 25 s
   failure, so the gate is wired to the mechanism and not passing for another reason.
2. **The `enabled = false` row is worse than this note first claimed.** It does not merely fail to
   run — it reports SUCCESS (`RepeaterChanged { enabled: true }`) while the spawned thread returns
   `Ok(0)` instantly at `run_full_duplex`'s `if !self.config.enabled` guard. That is #1298's exact
   class surviving inside the arm #1298 hardened, because #1298 checked only whether a repeater was
   AVAILABLE to take, never whether it would run.

So the remaining decision is solely the startup/lifecycle one (A vs B), and it is the only thing
standing between the plain `enable_repeater` path and a green gate.

## Review outcome (Fable, 2026-09-09) — A and B both rejected for C

The review confirmed all three facts and strengthened fact 1: `git log --all -S'run_full_duplex'`
shows the spawn has **never** lived anywhere but the `EnableRepeater` arm, and the
`repeater_enabled: cfg.repeater.enabled` line plus the "already enabled" check both pre-date it. So
the accurate statement is not "this predates #1308" but **"`[repeater] enabled = true` has never
been startable in the project's history."**

**Corrections to my framing, all of which I accept:**

1. **My "carrier sense" claim was unsupported and I have removed it from the Twins section below.**
   `grep -rni 'dcd|csma|carrier.sense|channel_busy' crates/openpulse-repeater/src` finds one comment
   about the *daemon's* receive-side DCD. `relay_burst_at` keys rig_b whenever a burst decodes, with
   no check that rig_b's band is clear. That is a real gap for an unattended transmitter — filed
   separately, not cited as an asset.
2. **The precedent I should have cited is the JS8 discovery beacon, not the monitor.** It is the
   other §97.221 automatic transmitter in this daemon, it starts from config alone
   (`build_discovery_runtime` → `DiscoverySm::new(params.enabled…)`, no `EnableDiscovery` needed),
   and `EnableDiscovery`/`DisableDiscovery` are the switch on top. That is shape A for a transmitter,
   already shipped and already mapped in `docs/regulatory.md`.
3. **The mesh precedent is not a counterweight.** `openpulse-mesh/tests/no_real_audio.rs` is explicit
   that the capability was removed for having no PTT, no carrier sense, no ID timer and no callsign
   — not for autostarting.
4. **§97.221 does not favour B.** Its requirements (a control point that can terminate; ID) are met
   identically either way. B would make the repeater attended-only — dead after any power cycle
   until a human reaches the control port — which is "the feature does not exist for its intended
   deployment".
5. **A alone does not fix claim 3.** Under A, `enabled = false` + `enable_repeater` still spawns a
   thread that exits at once and reports success, because `RepeaterConfig.enabled` gates the loop.
   A and B conflate two independent questions: does startup spawn, and what does the *crate's* flag
   gate.

**Adopted: option C** — config = autostart, command = runtime toggle, the two independent:
1. startup spawns when `cfg.repeater.enabled`;
2. the `config.enabled` checks come out of `run_full_duplex` and `relay_burst_at` — the daemon owns
   the lifecycle, so a repeater it chose to run runs;
3. `RepeaterConfig.enabled` is dropped from the repeater crate (it would gate nothing), and the
   test that used `enabled = false` as its "thread exits" stand-in now drops the burst sender, which
   is the honest stand-in and what a shutdown actually does.

The spawn is extracted into one `spawn_repeater` helper called by both entry points, because the
defect was precisely that those two paths had drifted.

**Also corrected here:** the Consumer field named only the `lib.rs` arm, not the production drivers
(CLI `daemon enable-repeater`, panel `ToggleRepeater`); and the Twins field's "no sibling front-end
with this gap" was true for construction but false for consumption — the panel is a twin that is
*worse* off, since it starts from `repeater_enabled: false`, only updates on `RepeaterChanged`, and
so can never send the `Disable` that the CLI-only workaround required.

**Deferred, filed rather than absorbed:** the single-use repeater (`DisableRepeater` consumes it, so
an operator cannot re-enable without a daemon restart — the thread could return it via
`JoinHandle<CrossBandRepeater>`), and the missing rig_b carrier sense.
