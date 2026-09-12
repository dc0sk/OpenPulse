#!/usr/bin/env bash
# Re-homed doc / attribute lint (#1345). A wrapper around scripts/lib/rehomed_docs.py.
#
# A diff can move a doc comment or an outer attribute onto the wrong item without touching either,
# because rustdoc attaches every consecutive `///` line (and an outer `#[...]`) to the next item.
# Nothing in the toolchain sees the contiguous form: clippy's `empty_line_after_doc_comments` catches
# only a blank line between doc and item. The #1345 census found 29 live insertion steals (30 hits,
# one a false positive), plus lost summaries, orphaned field docs and re-homed `#[cfg]`s, all under a
# green gate.
#
# This lints the diff from the merge-base of HEAD and the base ref, judging the NEW file. Design and
# its two review rounds: docs/dev/design/rehomed-docs-check.md.
#
# Usage:  scripts/check-rehomed-docs.sh [--base REF]   # REF defaults to origin/main
#         scripts/check-rehomed-docs.sh --self-test
# Exit:   0 clean, 1 re-homed doc/attribute found, 2 the lint could not run (never read as clean).
#
# FAIL CLOSED ON THE BASE, following check-trailer.sh (#1219). An unresolvable base must not read as
# an empty, and therefore clean, range. There is also no fallback ref: `|| echo HEAD` diffs HEAD
# against itself and passes vacuously, which is the check-review.sh defect the design review found.
set -uo pipefail
REPO_ROOT="$(git rev-parse --show-toplevel)" || exit 2
cd "$REPO_ROOT" || exit 2
PY="$REPO_ROOT/scripts/lib/rehomed_docs.py"
SELF="$REPO_ROOT/scripts/check-rehomed-docs.sh"

if [ "${1:-}" = "--self-test" ]; then
    python3 "$PY" --self-test || exit 1
    # RECALL, not just the fixture table. The fixtures test shapes the author thought of, and that is
    # exactly how the walk-up shipped blind to `//` comments and wrapped attributes — absent from
    # history AND from the fixtures, so the replay and the self-test agreed with each other and with
    # nothing else. This plants the same steal at real sites at HEAD, in four shapes, and carries its
    # own control: the pre-review walk-up must MISS some of them.
    python3 "$PY" --recall 12 || exit 1
    # P8 — base handling. A well-formed 40-hex object NAME satisfies `rev-parse --verify`, so the
    # wrapper must test `^{commit}`; both bad forms must FAIL, and a resolvable base must still lint.
    if "$SELF" --base "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" >/dev/null 2>&1; then
        echo "SELF-TEST: FAIL — an unresolvable base was read as a clean range"; exit 1
    fi
    if "$SELF" --base "origin/no-such-branch-for-self-test" >/dev/null 2>&1; then
        echo "SELF-TEST: FAIL — a nonexistent base ref was read as a clean range"; exit 1
    fi
    if ! "$SELF" --base HEAD >/dev/null 2>&1; then
        echo "SELF-TEST: FAIL — a resolvable base was rejected"; exit 1
    fi
    echo "SELF-TEST: PASS — covered forms, slide discrimination, fail-closed base"; exit 0
fi

base_ref="origin/main"
if [ "${1:-}" = "--base" ]; then
    base_ref="${2:-}"
    if [ -z "$base_ref" ]; then
        echo "usage: scripts/check-rehomed-docs.sh [--base REF] | --self-test" >&2; exit 2
    fi
elif [ -n "${1:-}" ]; then
    echo "usage: scripts/check-rehomed-docs.sh [--base REF] | --self-test" >&2; exit 2
fi

if ! git rev-parse --verify --quiet "${base_ref}^{commit}" >/dev/null 2>&1; then
    echo "rehomed-docs: base '$base_ref' does not resolve to a commit in this checkout." >&2
    echo "              Refusing to lint: an unresolvable base yields an empty range, which is" >&2
    echo "              indistinguishable from a clean one." >&2
    echo "REHOMED-DOCS: FAIL"
    exit 2
fi
if ! mb="$(git merge-base "$base_ref" HEAD 2>/dev/null)"; then
    echo "rehomed-docs: '$base_ref' and HEAD share no history; refusing to lint." >&2
    echo "REHOMED-DOCS: FAIL"
    exit 2
fi

# Print the range, so an empty one (a branch with nothing new) is visible rather than silently green.
echo "rehomed-docs: linting $(git rev-parse --short=8 "$mb")..$(git rev-parse --short=8 HEAD) (merge-base of $base_ref and HEAD)"
python3 "$PY" "$mb" HEAD
rc=$?
case $rc in
    0) echo "REHOMED-DOCS: PASS" ;;
    1) echo "REHOMED-DOCS: FAIL — move each doc or attribute back above the item it describes (the 'owner' line names it)" ;;
    *) echo "REHOMED-DOCS: FAIL — the lint could not run (exit $rc)"; rc=2 ;;
esac
exit $rc
