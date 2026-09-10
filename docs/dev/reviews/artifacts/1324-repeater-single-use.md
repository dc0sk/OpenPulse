# #1324 — the repeater is single-use: `DisableRepeater` consumes it

## The defect

`spawn_repeater` does `runtime_state.repeater.take()?` and moves the repeater into a thread whose
closure returns `()`. `DisableRepeater` sets the stop flag and `let _ = thread.join();` — the
repeater is dropped with the closure. So:

1. daemon starts with `[repeater] enabled = true` → running;
2. `disable-repeater` → stops cleanly, `RepeaterChanged { enabled: false }`;
3. `enable-repeater` → `CommandError`: *"no repeater is available — it was not built at startup …
   or a previous session ended and consumed it"*.

Only a daemon restart recovers. For an unattended §97.221 station that is the wrong shape: the
control point can stop the transmitter (the regulatory requirement, satisfied) but cannot start it
again without shell access to the host. Note this got *worse* in practice once #1326 made the
repeater startable at all and #1328 gave it a reason to stop on its own (`MAX_SENSE_FAULTS`).

Two join sites drop it: `reap_finished_repeater` (lib.rs:1657) and the `DisableRepeater` arm
(lib.rs:2738).

## Option A — the thread hands it back

`JoinHandle<CrossBandRepeater>`: the closure returns the repeater, and both join sites put it back
into `runtime_state.repeater`. `CrossBandRepeater` is `Send` again since #1328 moved the
`CaptureTicker` (which holds a `!Send` `cpal::Stream`) out of the struct and into `run_full_duplex`,
so this compiles today; it would not have before that change.

## Option B — rebuild on demand

Store what construction needs (config + a backend factory + the rig_b PTT builder) and build a fresh
repeater on every `EnableRepeater`. No object survives a session, so no state can be carried into
one. Costs: the rig_b PTT controller is built once at startup today and its construction is where
#1260's alias refusal lives, so B either rebuilds that too (re-running a refusal at runtime that is
currently a startup-only decision) or keeps the PTT and rebuilds only the engines.

## My recommendation

**A**, for one reason that is about behaviour rather than tidiness: the repeater's `SharedPtt` owns
rig_b's key and a watchdog, and rebuilding it (B) means tearing down and re-establishing the keying
path of a transmitter that may be mid-release. A hands back the same PTT, untouched.

## The part I am least sure about, and want tested

**Stale bursts.** The bounded channel holds up to 4 flushed bursts. The daemon's rx tick only
`try_send`s while `repeater_stop.is_some()`, so nothing accumulates *while disabled* — but bursts
queued in the moments before a disable survive in the channel and would be relayed on re-enable,
putting seconds-to-minutes-old audio on the air. I believe the re-enable path must **drain the
channel** before the thread starts. I have not measured how many bursts actually survive a disable,
and I do not know whether draining belongs in `spawn_repeater` (every start) or only in the
resume path.

**Error-exit disposition.** Should a repeater whose session ended in an error (e.g. #1328's
`MAX_SENSE_FAULTS`, meaning it could not hear its output band) be handed back for re-enabling? I
lean yes — the operator fixes the rig and retries, and the daemon has already reported the reason —
but "hand back the object that just failed" is exactly the shape that hides a latent fault, and the
counter-argument is that #1285's ruling refuses to start on an unusable PTT rather than degrading.

## Consumer

`crates/openpulse-daemon/src/lib.rs:2738` (`DisableRepeater` arm) and `:1657`
(`reap_finished_repeater`) — the two sites that join the thread and discard the repeater. Driven in
production by the CLI `openpulse daemon disable-repeater` / `enable-repeater`
(`crates/openpulse-cli/src/commands/daemon.rs:81-82`) and by the panel's repeater toggle. The panel
matters here: it starts from `repeater_enabled: false` and only updates on `RepeaterChanged`, so an
operator who toggles it off currently cannot get it back from the GUI at all.

## Prior art

- **#1298** built `reap_finished_repeater` on precisely the consumed-repeater semantics: it clears
  the flags when a thread exits so the next command sees the truth. Whatever lands must keep "the
  thread died" and "the operator stopped it" distinguishable, which is the property #1298 bought.
- **#1326** extracted `spawn_repeater` as the single start path for both startup and the command;
  a restore path should have the same shape, i.e. one function, not two copies.
- `crates/openpulse-daemon/src/ptt.rs:139` `spawn_watchdog` returns `JoinHandle<()>` and is the
  daemon's other long-lived thread — it owns no reusable object, so it is not a precedent either way.
- `grep -n 'JoinHandle<' crates/openpulse-daemon/src/*.rs`: every other handle in the daemon is
  `JoinHandle<()>` (spectrum tasks, twin harness). **There is no existing example in this crate of a
  thread handing an object back**, so option A introduces a pattern rather than following one.

## Twins

- **`DisableRepeater` and `reap_finished_repeater` are twins of each other** and both drop the
  repeater. Fixing one and not the other would leave the error-exit path still consuming it — the
  #1252 shape (one arm pinned, the sibling left open).
- **The discovery runtime** is the other config-enabled service, but it lives on the async side and
  is not moved into a thread, so it has no equivalent hazard.
- **`repeater_bursts`**: the `SyncSender` lives in `RuntimeControlState` and survives disable/enable,
  while the `Receiver` travels with the repeater. Any option that rebuilds the repeater (B) must
  rebuild the channel too, or the new repeater listens on a receiver nobody sends to — a silent
  dead relay, which is exactly the #1308 defect class this all started from.

## Review outcome (Fable, 2026-09-09) — A adopted, but for a different reason, and my history was false

**Corrections to my framing, all verified:**

1. **"`CrossBandRepeater` is `Send` only since #1328" is FALSE.** `CaptureTicker` was never a
   committed struct field — `git log -S'sensor: Option<CaptureTicker>'` returns nothing, and
   `git show fcbb8594:…/lib.rs | grep -c CaptureTicker` is 0. I was describing my own uncommitted
   intermediate state as project history. The type was already `Send` at #1326, necessarily so:
   `std::thread::spawn` carries the same `Send + 'static` bound as `JoinHandle<T>`, so if the
   repeater could be moved into the thread it could always be handed back. My `assert_send` probe
   proves `Send` **today**; it proves nothing about the causal history, and I presented it as if it
   did. (Also: `Box<dyn AudioInputStream>` is `!Send` by the trait-object type alone, not because of
   `cpal::Stream`.)
2. **"Two join sites drop it" is imprecise.** The drop is in the closure, the moment
   `run_full_duplex` returns; joining only *observes* it. That matters for A: the repeater is parked
   in the finished thread's packet until someone joins, so reap must actually join — it does.
3. **"Mid-release" overstates the B hazard.** `run_full_duplex` sets `session_guard = None` before
   returning and the guard's `Drop` releases synchronously, so nothing is mid-release at join. The
   defensible version is narrower: a *failed* hardware release leaves the deadline armed and relies
   on the watchdog retrying, and the watchdog holds only a `Weak` and exits when the last
   `SharedPtt` drops — so a full rebuild would destroy the one thing that would ever unkey a stuck
   rig_b, and `RigctldPtt` has no `Drop`. That applies only to B-rebuild-everything, not to the
   "keep the PTT, rebuild the engines" variant, so my "A for one reason" was not decisive.
4. **The real reason for A is the §97.119 ID timer, which I never mentioned.**
   `StationIdTimer`'s ID **clock** carries across a pause under A. B calls
   `StationIdTimer::new(interval, now)`, which re-seeds `last_id_ms = now`, so the next ID is
   deferred by up to a **full interval** measured from the rebuild.

   **CORRECTED 2026-09-10.** This originally said the carried field was `tx_since_id`, and that a
   rebuild "forgets that un-IDed transmissions happened". Measured: `tx_since_id` is written and read
   inside the same call — `maybe_identify` has one caller, immediately after the `transmit` that
   armed it — so its value across a pause is unobservable. The conclusion (prefer A) is unchanged and
   the §97.119 argument still carries it; the FIELD was wrong. A rebuild that carried `last_id_ms`
   forward would not have this defect, which also changes the rebuild option's cost.
5. **The stale-burst hazard is a property of OPTION A, not of the system.** Under B with a rebuilt
   channel it vanishes. My Twins section listed the channel only as a B cost and the note framed the
   hazard as universal.
6. **My probe's "during-stop" leg is vacuous by construction** — the loop tests `stop` before
   `recv_timeout`, so a pre-set flag can never consume anything. The conclusion stands on the
   re-enable leg alone, which is sound.
7. **"No state can be carried" (B) was stated as a virtue without listing what A carries:**
   `sense_faults`, `bursts_deferred`, the ID timer, `engine_tx`'s DCD tracker, and the burst queue.
   Two of those are advantages; one is a footgun (below). The note named none.

**Adopted: A**, with four things the note did not have:

- **Drain the queue at the top of `run_full_duplex`, every start, with a discarded-count tripwire.**
  The daemon cannot drain a receiver it does not hold; the first start is a no-op by construction
  (the rx tick sends only while `repeater_stop.is_some()`, on the same task as the command handler);
  and it covers the error-exit path without a second copy.
- **Reset `sense_faults` per session.** It is a struct field reset only by a `Clear` verdict, so a
  repeater handed back after `MAX_SENSE_FAULTS` would start its next session with a fault budget of
  **1, not 10**, until it happened to hear a clear band. This is the concrete "state carried in the
  returned object", and it was undocumented and untested.
- **Hand it back even after an error exit.** #1285's ruling does not apply: that refuses a *silent
  degrade at start*, whereas this failure is loud (`CommandError` + `RepeaterChanged`), the
  transmitter is off, and restarting needs an explicit control-point command. There must never be an
  auto-retry — that would be the blind keying #1328 exists to stop — and the re-enable path already
  opens a fresh sensor, which is exactly the retry an operator who fixed a cable wants.
- **Three adjacent defects this touches**, all confirmed by reading: (a) an error exit emits **two**
  `RepeaterChanged { false }` edges, one from the thread and one from reap, while the comment claims
  it deliberately emits only one; (b) `engine.set_relay_mode(None)` never runs on an error exit, so
  the burst cap stays oversized; (c) `run_full_duplex`'s `Disconnected` comment says "or the
  repeater was disabled", which is false — disable never drops the sender.

**Filed, not absorbed:** option **C** (a long-lived thread taking Enable/Disable as messages) is the
right answer to a different problem — today's `join` **blocks the `select!` loop**, so a disable
stalls rx ticks, commands and event forwarding for the remainder of an in-flight relay, which at
BPSK31 is a full frame's airtime. That is pre-existing and A keeps it. Also filed: `AudioSamples`
carries no timestamp, so even in steady state a 4-deep queue behind a ~68 s BPSK31 relay permits
~3–4 minutes of staleness — the drain fixes the unbounded case, an age stamp would fix the general
one.
