#!/usr/bin/env bash
# The workspace gate. Run this instead of composing cargo invocations by hand.
#
# WHY THIS EXISTS. Five separate times, a verdict about this repo was wrong because the CHANNEL
# carrying it was corrupted, not because the code was:
#   - `cargo test ... | tail -3` then `$?`   -> captured tail's status; a failed build reported green
#   - `${PIPESTATUS[0]}`                     -> bash-only; empty under zsh, silently
#   - `| tail -20` on a results summary      -> failures earlier in the run invisible
#   - no `--no-fail-fast`                    -> cargo stops at the first failing BINARY; the count
#                                               is a lower bound, never a total (bitten twice)
#   - "main is clean"                        -> asserted with no run behind it at all
#
# So this script bans the constructs rather than asking anyone to use them carefully:
#   * no pipelines anywhere in the verdict path — every status comes from `cmd > log 2>&1; rc=$?`,
#     the only form that is shell-dialect-free
#   * full output to a log file; the terminal gets a summary, but the FAILURE LIST IS NEVER
#     TRUNCATED, because failures are the one thing that must not be cut
#   * a machine-checkable last line (`GATE: PASS|FAIL|PARTIAL|INVALID ...`) and
#     `target/gate-verdict.json`. INVALID (exit 3) means the tree or HEAD moved during the
#     run, so the verdict is not attributable to any single state of the repo — rerun it;
#     it is NOT a code failure and must not be chased as one.
#
# SABOTAGE-VERIFY THIS SCRIPT AFTER EVERY EDIT. A gate nobody has watched fail is exactly the
# self-consistent checker it exists to prevent — this repo's archetype #1 wearing a safety vest.
#   1. plant a failing assertion in any test
#   2. run this script
#   3. require `GATE: FAIL` with that test named in the failure list
#   4. revert
# `--self-test` automates a NARROWER version of that against a scratch test file: it plants the
# fixture, runs `cargo test` DIRECTLY, and requires a non-zero status with the planted test named.
# It does not go through run_step and exits before any verdict is written, so it prints no `GATE:`
# line and writes no gate-verdict.json — the failure-detection path only. CLAUDE.md rule 3 claimed
# it "requires GATE: FAIL" until #1242; it has never printed one. A change to the VERDICT path is
# therefore invisible to it: `python3 scripts/lib/trace.py evidence-self-test` is what covers that.
#
# Usage:
#   scripts/gate.sh            # fmt + clippy + full workspace test
#   scripts/gate.sh --quick    # fmt + clippy only (NOT a gate; prints GATE: PARTIAL, no token)
#   scripts/gate.sh --self-test

set -u

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO_ROOT" || exit 2

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOG_DIR="$REPO_ROOT/target"
LOG="$LOG_DIR/gate-$STAMP.log"
VERDICT="$LOG_DIR/gate-verdict.json"
mkdir -p "$LOG_DIR"

MODE="full"
case "${1:-}" in
    --quick) MODE="quick" ;;
    --self-test) MODE="self-test" ;;
    # Prints the drift fingerprint and exits. Exists so the primitive can be tested in seconds
    # instead of only through a 2 h run — the properties everything else rests on (content edit
    # changes it, mtime alone does not, writes under target/ do not, an untracked file's CONTENT
    # does) are each one command with this.
    --fingerprint) MODE="fingerprint" ;;
    "") ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
esac

COMMIT=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
if [ -n "$(git status --porcelain 2>/dev/null)" ]; then DIRTY="dirty"; else DIRTY="clean"; fi

# Run one command, capture its REAL status. No pipes: `$?` after a pipeline is the last element's
# status, and ${PIPESTATUS}/$pipestatus differ between bash and zsh. This form works everywhere.
run_step() {
    step_name=$1; shift
    printf '  %-38s' "$step_name"
    echo "=== $step_name: $* ===" >> "$LOG"
    "$@" >> "$LOG" 2>&1
    rc=$?
    # Completion marker (#1224). Without it a truncated log is indistinguishable from a finished one
    # — a completed gate log simply ends with the last step's output — so `trace.py` read a
    # half-written log as evidence and reported CITED-BUT-DIDN'T-RUN for tests that had not been
    # reached yet. Written REGARDLESS of $rc: a test run with failures is still a completed run and
    # is valid evidence about which tests passed; only truncation invalidates it.
    echo "=== end $step_name: exit $rc ===" >> "$LOG"
    if [ "$rc" -eq 0 ]; then echo "ok"; else echo "FAILED (exit $rc)"; fi
    return $rc
}

self_test() {
    scratch="crates/openpulse-core/tests/gate_self_test_sabotage.rs"
    # Remove the fixture even if the run is interrupted. An abandoned sabotage file was left in the
    # tree once by a Ctrl-C'd self-test, and a deliberately-failing test sitting in a source dir is
    # exactly the thing nobody expects to find there.
    trap 'rm -f "$scratch"' EXIT INT TERM
    echo "SELF-TEST: planting a deliberately failing test at $scratch"
    cat > "$scratch" <<'RS'
// Temporary sabotage fixture written by scripts/gate.sh --self-test. Safe to delete.
#[test]
fn gate_self_test_must_fail() {
    assert_eq!(1, 2, "deliberate failure: if the gate reports PASS with this present, it is broken");
}
RS
    out="$LOG_DIR/gate-selftest-$STAMP.log"
    cargo test --workspace --no-default-features --no-fail-fast > "$out" 2>&1
    rc=$?
    rm -f "$scratch"
    if [ "$rc" -ne 0 ] && grep -q "gate_self_test_must_fail" "$out"; then
        echo "SELF-TEST: PASS — the gate detected the planted failure (exit $rc)"
        return 0
    fi
    echo "SELF-TEST: FAIL — planted failure was NOT detected (exit $rc). The gate is not a gate."
    echo "           full output: $out"
    return 1
}

if [ "$MODE" = "self-test" ]; then self_test; exit $?; fi

# ---------------------------------------------------------------------------- drift guard (#1151)
#
# WHAT THIS DETECTS, AND WHAT IT DOES NOT. `COMMIT` and `DIRTY` above are read once, before step 1,
# while every step resolves HEAD or scans the tree at ITS OWN step time — a ~2 h run gives HEAD and
# the tree plenty of time to move, and one real verdict already named a commit made 39 minutes in on
# a different branch. This guard samples the tree and HEAD at every step boundary and voids the run
# on a change.
#
# It catches drift that PERSISTS to a sample point. It does NOT catch a mutate-and-revert inside a
# single step: two of the incidents that motivated it were sub-second edits that would hash
# identically at both boundaries. Detecting those needs a filesystem watcher whose own liveness is
# verified — #1231. Do not write "the gate detects mid-run mutation" anywhere; it detects
# PERSISTENT mid-run mutation.
#
# The fingerprint is a temp-index `write-tree`, NOT a hash of `git status` + `git diff`. Those are
# blind to the CONTENT of untracked files — status prints `?? path` and diff prints nothing — and an
# untracked file is the likeliest mutation (the self-test's own fixture is one). The real index is
# copied first so stat-clean files reuse their cached hashes instead of re-hashing the tree. HEAD is
# compared separately: `git commit` mid-run changes no working-tree content but moves HEAD, and the
# trailer and review lints read commits.
#
# `target/` is gitignored, so the gate's own log writing does not register — verify that stays true
# if .gitignore changes, because an unignored target/ would make this fire on every run.
fingerprint() {
    _idx=$(mktemp) || return 1
    cp .git/index "$_idx" 2>/dev/null || :
    GIT_INDEX_FILE="$_idx" git add -A >/dev/null 2>&1
    GIT_INDEX_FILE="$_idx" git write-tree 2>/dev/null
    rm -f "$_idx"
}

if [ "$MODE" = "fingerprint" ]; then fingerprint; exit 0; fi

START_TREE=$(fingerprint)
START_HEAD=$(git rev-parse HEAD 2>/dev/null || echo "unknown")

# Called BEFORE each step and once after the last. Aborting at the next boundary rather than at the
# end keeps a 2 h test step from being spent on a run that is already void — and it is what makes
# the full-path sabotage cheap: mutate during the seconds-long fmt step and the gate aborts before
# clippy starts. It lives here rather than inside run_step, whose contract is running ONE command;
# drift policy is sequencing, not step execution.
drift_check() {
    _tree=$(fingerprint)
    _head=$(git rev-parse HEAD 2>/dev/null || echo "unknown")
    echo "=== tree $_tree head $_head ===" >> "$LOG"
    [ "$_tree" = "$START_TREE" ] && [ "$_head" = "$START_HEAD" ] && return 0
    _what=""
    [ "$_tree" != "$START_TREE" ] && _what="working tree"
    [ "$_head" != "$START_HEAD" ] && _what="${_what:+$_what and }HEAD"
    _steps=$([ "${rc_total:-0}" -eq 0 ] && echo PASS || echo FAIL)
    cat > "$VERDICT" <<JSON
{
  "result": "INVALID",
  "reason": "$_what changed during the run",
  "steps_result": "$_steps",
  "commit": "$START_HEAD",
  "commit_end": "$_head",
  "tree_start": "$START_TREE",
  "tree_end": "$_tree",
  "timestamp": "$STAMP",
  "log": "$LOG"
}
JSON
    echo
    echo "  The $_what changed while the gate was running, so this run proves nothing about any"
    echo "  single state of the repo. Steps so far reported $_steps — not discarded, but not"
    echo "  attributable either. Rerun on a quiet checkout; put concurrent work in a git worktree."
    echo "GATE: INVALID $START_HEAD $_what $STAMP (steps reported $_steps)"
    exit 3
}

echo "gate: commit $COMMIT ($DIRTY)  log $LOG"
rc_total=0
drift_check
run_step "cargo fmt --check" cargo fmt --all -- --check || rc_total=1
drift_check
run_step "cargo clippy -D warnings" cargo clippy --workspace --no-default-features --all-targets -- -D warnings || rc_total=1

TEST_CMD="none"
if [ "$MODE" = "full" ]; then
    TEST_CMD="cargo test --workspace --no-default-features --no-fail-fast"
    drift_check
    run_step "cargo test (workspace)" cargo test --workspace --no-default-features --no-fail-fast || rc_total=1
    # Traceability is checked INSIDE the gate, not a separately-disableable job: enforced
    # requirements with no in-code binding, cited code/tests that don't exist, REQ<->CAP
    # disagreement, a capability that satisfies a requirement while citing no test (EMPTY-CAP,
    # #1268), and NEW code orphans beyond the grandfathered baseline.
    drift_check
    run_step "trace check (requirements)" env GATE_LOG="$LOG" scripts/trace.sh check || rc_total=1
    # Reachability ratchet: a NEW public item no production code references (coverage would vouch
    # for it as "covered" if a test touches it). Grandfathered baseline; only growth fails.
    drift_check
    run_step "reachability ratchet" scripts/reachability.sh check || rc_total=1
    # Requirements-trailer lint on this branch's commits. Here so a local `GATE: PASS` predicts CI:
    # traceability.yml runs the same check on every PR, and a gate that green-lights a push CI then
    # rejects has stopped meaning anything. Inspects COMMITS only, so a dirty tree mid-work does not
    # trip it; `--quick` skips it with the rest. (The PR *body* half of this check — the squash
    # message that actually lands on main — can only run in CI, where the body exists.)
    drift_check
    run_step "requirements-trailer lint" scripts/check-trailer.sh || rc_total=1
    # Ledger ordering. The file declares "Newest first" and had drifted into two regimes — 12 breaks
    # across 359 entries — because the convention lived only in prose and nothing measured it. Cheap
    # (no build, no I/O beyond one file), so it costs nothing to keep honest.
    drift_check
    run_step "ledger ordering" scripts/check-ledger-order.sh || rc_total=1
    # Secondary host only. The PR-body lint in traceability.yml is what actually enforces the
    # review trailer; this reports the classification locally so a design-class branch is known
    # to need an artifact BEFORE the PR is opened.
    drift_check
    run_step "review-trailer lint" scripts/check-review.sh --base "$(git merge-base HEAD origin/main 2>/dev/null || echo HEAD)" || rc_total=1
    # Re-homed doc / attribute lint (#1345). A diff can move a `///` doc or an outer `#[...]` onto the
    # wrong item without touching either, and clippy only sees the blank-line form; 29 live insertion
    # steals sat under a green gate. The script resolves the merge-base itself and FAILS on an
    # unresolvable base; there is no `|| echo HEAD` fallback, because that diffs HEAD against itself
    # and passes vacuously.
    drift_check
    run_step "re-homed docs lint" scripts/check-rehomed-docs.sh || rc_total=1
fi

drift_check   # final boundary: nothing moved between the last step and the verdict

# Counts come from the LOG FILE, never from a pipe carrying the runner's output.
passed=$(awk '/^test result:/ {for(i=1;i<=NF;i++) if($i=="passed;") s+=$(i-1)} END {print s+0}' "$LOG")
failed=$(awk '/^test result:/ {for(i=1;i<=NF;i++) if($i=="failed;") s+=$(i-1)} END {print s+0}' "$LOG")
# `grep -c` prints 0 AND exits 1 when there are no matches, so `|| echo 0` appends a SECOND line
# and `suites` becomes "0\n0" — which produces invalid JSON in gate-verdict.json. Harmless only
# because --quick returns before the JSON write; fixed rather than left as a latent trap.
suites=$(grep -c '^test result:' "$LOG" 2>/dev/null | head -1)
suites=${suites:-0}

# The failure list is printed IN FULL. Truncating it is the defect this script exists to prevent.
if [ "$failed" -gt 0 ] || [ "$rc_total" -ne 0 ]; then
    echo ""
    echo "FAILURES (complete, untruncated):"
    awk '/^failures:$/{f=1;next} /^test result:/{f=0} f && /^    [a-zA-Z0-9_:]+$/{print "  " $1}' "$LOG" | sort -u
    grep -E '^error(\[|:)' "$LOG" | sort -u | sed 's/^/  /'
fi

echo ""
echo "suites=$suites tests_passed=$passed tests_failed=$failed"
# A green gate must not read as "everything passed". Two acceptance suites are held out for
# runtime (#1274) — REQ-QRM-01's notch gate and the OTA rate-adaptation suite, ~83 min between
# them — and nothing else in this output would say so.
echo "held-out (runtime, #1274): notch_rescues_interferer, ota_channel_adaptation — run scripts/slow-tests.sh"

if [ "$MODE" = "quick" ]; then
    echo "GATE: PARTIAL $COMMIT $DIRTY $STAMP (fmt+clippy only — NOT a gate, no token written)"
    drift_check   # --quick shares the guard by reference; a drifted PARTIAL is quoted too
    exit $rc_total
fi

if [ "$rc_total" -eq 0 ] && [ "$failed" -eq 0 ]; then
    result="PASS"
else
    result="FAIL"
fi

# The toolchain is part of what a verdict is attributable to, and it is the member that drifts
# WITHOUT anyone performing an act: a distro package upgrade re-derives every cached lint verdict.
# The pin in rust-toolchain is unenforceable on this host (no rustup), so this record is the only
# thing that makes that drift visible to anything reading the verdict later.
TOOLCHAIN=$(rustc -V 2>/dev/null || echo "unknown")

cat > "$VERDICT" <<JSON
{
  "result": "$result",
  "commit": "$COMMIT",
  "tree": "$DIRTY",
  "toolchain": "$TOOLCHAIN",
  "timestamp": "$STAMP",
  "command": "$TEST_CMD",
  "suites": $suites,
  "tests_passed": $passed,
  "tests_failed": $failed,
  "log": "$LOG"
}
JSON

echo "GATE: $result $COMMIT $DIRTY $STAMP"
[ "$result" = "PASS" ] && exit 0 || exit 1
