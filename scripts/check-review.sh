#!/usr/bin/env bash
# Adversarial-review trailer lint.
#
# Usage: scripts/check-review.sh --message-file FILE --base REF     # lint a PR body (primary)
#        scripts/check-review.sh --base SHA                         # lint the local branch's diff
#        scripts/check-review.sh --self-test
#
# TWO TIERS, and the split is the whole design:
#
#   Tier 1 (every PR, one line). The body carries `Review: <path>` or `Review: none — <reason>`.
#     This converts a silent omission into an explicit recorded claim. `Review: none` is a legitimate
#     answer — most PRs are mechanical — but it is now a claim somebody wrote, greppable after the
#     fact (`git log --grep 'Review: none'`), rather than an absence nobody can see.
#
#   Tier 2 (design-class PRs). If the diff touches a decision site, `Review: none` FAILS and the
#     named artifact must exist and pass a structure check.
#
# WHY A PR-BODY LINT AND NOT A GATE STEP: the workspace gate does not run at merge (ci.yml scopes
# every job to release/** — issue #1144), takes 80-120 minutes, and is required on no ref. A check
# that fires weeks later fires after the author is gone. `traceability.yml` runs on EVERY PR
# including the `edited` event, which is where this belongs and where the sibling trailer lint
# demonstrably worked. gate.sh runs it too, for the local pre-push case only.
#
# WHAT THIS CANNOT DO: it cannot tell a real review from a fabricated one. Requiring artifact
# CONTENT converts forgetting into fabricating — a deliberate act rather than a lapse — and that is
# the ceiling for any checker that inspects only the repo. The provenance defence is that the review
# apparatus writes the artifact itself, so faking one means faking the apparatus.
#
# THE CLASSIFIER IS STRUCTURAL ON PURPOSE. An earlier draft hand-listed "wire-format files"; that
# would have re-committed the exact defect #1193 was about — a hand-maintained list, inside the
# checker written because a hand-maintained list rotted four times. Design-class is derived from
# where decisions are RECORDED (a design doc, a new module, the requirements registry), not from a
# list of files somebody must remember to extend.
set -u
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO_ROOT" || exit 2

REVIEW_DIR="docs/dev/reviews"
MIN_ARTIFACT_BYTES=1200   # a stub passes a "file exists" check; it must not pass this one

MSG_FILE=""
BASE=""
SELF_TEST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --message-file) MSG_FILE="$2"; shift 2 ;;
        --base)         BASE="$2"; shift 2 ;;
        --self-test)    SELF_TEST=1; shift ;;
        *)              BASE="$1"; shift ;;
    esac
done

# Decision sites. Structural, not a file list:
#   docs/dev/design/**          — where a design decision is written down
#   */src/**       (NEW files)  — a new module is a structural decision
#   scripts/**     (NEW files)  — a new script is usually a new gate, i.e. a decision about how the
#                                 project verifies itself. This rule makes the classifier
#                                 SELF-CONSISTENT: it classifies its own introduction as
#                                 design-class, which is the least it should do.
#   .github/workflows/** (NEW)  — same reasoning, for CI
#   requirements.yaml           — the requirement/capability registry
is_design_class() {   # reads "STATUS<TAB>path" lines (git diff --name-status) on stdin
    awk -F'\t' '
        $2 ~ /^docs\/dev\/design\// { found = 1 }
        $2 ~ /^docs\/dev\/project\/requirements\.yaml$/ { found = 1 }
        $1 == "A" && $2 ~ /^(crates|plugins|apps|tools|pki-tooling)\/.*\/src\/.*\.rs$/ { found = 1 }
        $1 == "A" && $2 ~ /^scripts\// { found = 1 }
        $1 == "A" && $2 ~ /^\.github\/workflows\// { found = 1 }
        END { exit(found ? 0 : 1) }
    '
}

artifact_ok() {   # $1 = path ; echoes the reason it is bad, empty when good
    path="$1"
    [ -f "$path" ] || { echo "no such file"; return; }
    bytes=$(wc -c < "$path" | tr -d ' ')
    [ "$bytes" -lt "$MIN_ARTIFACT_BYTES" ] && { echo "only ${bytes}B; a review artifact under ${MIN_ARTIFACT_BYTES}B is a stub"; return; }
    grep -qiE '^##+[[:space:]]*prompt' "$path" || { echo "no '## Prompt' section — the artifact must record what was ASKED, or a reader cannot tell what was reviewed"; return; }
    grep -qiE '^##+[[:space:]]*verdict' "$path" || { echo "no '## Verdict' section — the artifact must record what came BACK"; return; }

    # The three cheap checks that, when skipped, made the reviewer do the proposer's falsification
    # (2026-09-06 lessons review). Every proposal overturned in that session was missing exactly one
    # of these, and each was one command away:
    #
    #   Consumer  — who CALLS this in production, by file:line. #1271 proposed answering a query from
    #               `GetConfig`, which runs on a task holding no engine; the design would have paid
    #               none of the debt it claimed. The consumer was never read.
    #   Prior art — the sweep for an existing mechanism, with its hits. #1268 proposed building a
    #               ratchet that already existed (`NOT-GRANDFATHERED`, trace.py). One grep.
    #   Twins     — the sibling paths that share the shape. #1252 pinned the responder and left the
    #               initiator open; #1177 and #1249 each needed a second arm.
    #
    # `UNCHECKED` is a legal answer and deliberately so: the point is to make the omission an
    # explicit written claim rather than an absence nobody can see, exactly as `Review: none` does at
    # tier 1. A field that may not be empty but may say UNCHECKED bans a construct; "consider the
    # consumer" would be an exhortation that cannot fail.
    for field in Consumer 'Prior art' Twins; do
        grep -qiE "^##+[[:space:]]*${field}" "$path" || {
            echo "no '## ${field}' section — a design artifact must record it, or the word UNCHECKED"
            return
        }
        # Non-empty: the heading alone is the omission wearing the fix's clothes.
        body=$(awk -v f="$field" '
            BEGIN { IGNORECASE = 1; want = "^##+[[:space:]]*" f }
            $0 ~ want { on = 1; next }
            on && /^##+[[:space:]]*/ { on = 0 }
            on { print }
        ' "$path" | tr -d '[:space:]')
        [ -n "$body" ] || {
            echo "'## ${field}' is empty — write what you found, or the word UNCHECKED"
            return
        }
    done
    echo ""
}

lint_message() {   # $1 = message text, $2 = design-class (0/1)
    msg="$1"; design="$2"
    review=$(printf '%s\n' "$msg" | sed -n 's/^Review:[[:space:]]*//p' | head -1)

    if [ -z "$review" ]; then
        echo "  FAIL: no 'Review:' trailer."
        echo "        Every PR records whether it was adversarially reviewed. Add ONE of:"
        echo "          Review: $REVIEW_DIR/<file>.md"
        echo "          Review: none — <why> (e.g. mechanical; applies a verdict already given)"
        return 1
    fi

    case "$review" in
        none*|None*|NONE*)
            if [ "$design" -eq 1 ]; then
                echo "  FAIL: 'Review: none' on a design-class change."
                echo "        This diff touches a decision site (a design doc, a NEW src module, or"
                echo "        requirements.yaml). The standing rule reviews those BEFORE implementation."
                echo "        Name the artifact: Review: $REVIEW_DIR/<file>.md"
                return 1
            fi
            reason=$(printf '%s' "$review" | sed 's/^[Nn][Oo][Nn][Ee][[:space:]]*[-—:]*[[:space:]]*//')
            if [ -z "$reason" ]; then
                echo "  FAIL: 'Review: none' with no reason."
                echo "        The point is a recorded claim, not a formality. Say why."
                return 1
            fi
            echo "  ok: Review: none — $reason"
            return 0
            ;;
    esac

    why=$(artifact_ok "$review")
    if [ -n "$why" ]; then
        echo "  FAIL: review artifact '$review' — $why"
        return 1
    fi
    echo "  ok: $review"
    return 0
}

if [ "$SELF_TEST" -eq 1 ]; then
    # A gate nobody has watched fail is the self-consistent checker it exists to prevent.
    tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
    rc=0

    printf 'A PR with no trailer at all.\n' > "$tmp/m1"
    if lint_message "$(cat "$tmp/m1")" 0 >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: a message with no Review: trailer was accepted"; rc=1
    else echo "  ok: missing trailer rejected"; fi

    printf 'body\n\nReview: none — mechanical\n' > "$tmp/m2"
    if lint_message "$(cat "$tmp/m2")" 1 >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: 'Review: none' was accepted on a DESIGN-CLASS change"; rc=1
    else echo "  ok: 'none' rejected on design-class"; fi

    printf 'stub\n' > "$tmp/stub.md"
    printf 'body\n\nReview: %s\n' "$tmp/stub.md" > "$tmp/m3"
    if lint_message "$(cat "$tmp/m3")" 1 >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: a STUB artifact was accepted"; rc=1
    else echo "  ok: stub artifact rejected"; fi

    # positive control: the checker must ACCEPT a good message, or it is vacuously strict
    printf 'body\n\nReview: none — applies a verdict already given\n' > "$tmp/m4"
    if lint_message "$(cat "$tmp/m4")" 0 >/dev/null 2>&1; then
        echo "  ok: a well-formed 'none' accepted (positive control)"
    else echo "SELF-TEST FAIL: a well-formed message was rejected"; rc=1; fi

    # positive control: a real artifact must pass. Note it carries the three proposal fields; if
    # this fixture is ever "fixed" by deleting them, every probe below still passes and the fields
    # stop being required — so the fixture IS part of the check.
    write_artifact() {   # $1 = path, $2.. = extra lines appended before ## Verdict
        {
            echo "# Review"
            echo "## Prompt"
            head -c 900 /dev/urandom | base64
            shift_done=0
            for extra in "${@:2}"; do echo "$extra"; done
            echo "## Verdict"
            echo ok
        } > "$1"
    }
    write_artifact "$tmp/real.md" \
        "## Consumer" "server.rs:1052 — the main loop, which holds the engine" \
        "## Prior art" "git grep -n NOT-GRANDFATHERED -> trace.py:684 (exists)" \
        "## Twins" "responder and initiator paths; both pinned"
    printf 'body\n\nReview: %s\n' "$tmp/real.md" > "$tmp/m5"
    if lint_message "$(cat "$tmp/m5")" 1 >/dev/null 2>&1; then
        echo "  ok: a structured artifact accepted on design-class (positive control)"
    else echo "SELF-TEST FAIL: a valid artifact was rejected"; rc=1; fi

    # 2026-09-06: each of the three proposal fields is required, and a HEADING ALONE does not
    # satisfy it. The empty-section probe is the one that matters — a checker that only greps for
    # the heading turns the requirement into a formatting rule the omission can wear.
    for missing in Consumer "Prior art" Twins; do
        args=()
        for f in Consumer "Prior art" Twins; do
            [ "$f" = "$missing" ] && continue
            args+=("## $f" "checked: see above")
        done
        write_artifact "$tmp/miss.md" "${args[@]}"
        printf 'body\n\nReview: %s\n' "$tmp/miss.md" > "$tmp/m_miss"
        if lint_message "$(cat "$tmp/m_miss")" 1 >/dev/null 2>&1; then
            echo "SELF-TEST FAIL: an artifact with no '## $missing' was accepted"; rc=1
        else echo "  ok: missing '## $missing' rejected"; fi
    done

    write_artifact "$tmp/empty.md" \
        "## Consumer" "" "## Prior art" "x" "## Twins" "y"
    printf 'body\n\nReview: %s\n' "$tmp/empty.md" > "$tmp/m_empty"
    if lint_message "$(cat "$tmp/m_empty")" 1 >/dev/null 2>&1; then
        echo "SELF-TEST FAIL: an EMPTY '## Consumer' section was accepted — the heading is not the check"; rc=1
    else echo "  ok: an empty proposal field rejected"; fi

    # UNCHECKED is legal on purpose: the goal is an explicit written claim, not a forced answer.
    write_artifact "$tmp/unchk.md" \
        "## Consumer" "UNCHECKED" "## Prior art" "UNCHECKED" "## Twins" "UNCHECKED"
    printf 'body\n\nReview: %s\n' "$tmp/unchk.md" > "$tmp/m_unchk"
    if lint_message "$(cat "$tmp/m_unchk")" 1 >/dev/null 2>&1; then
        echo "  ok: UNCHECKED accepted (an omission written down is the point)"
    else echo "SELF-TEST FAIL: UNCHECKED was rejected; it must be a legal answer"; rc=1; fi

    # #1219: an unresolvable base must FAIL the lint, never classify as "ordinary change".
    # All three shapes below passed before that fix. The middle one is the subtle one: a
    # well-formed 40-hex object NAME satisfies `git rev-parse --verify`, so the pre-existing
    # existence check did not fire and the failure fell through `git diff ... 2>/dev/null`.
    printf 'body\n\nReview: none — probe\n' > "$tmp/m6"
    for bad in "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" "origin/no-such-branch-for-self-test" ""; do
        if "$REPO_ROOT/scripts/check-review.sh" --message-file "$tmp/m6" --base "$bad" >/dev/null 2>&1; then
            echo "SELF-TEST FAIL: unresolvable base '${bad:-<empty>}' was treated as non-design-class"; rc=1
        else echo "  ok: unresolvable base rejected (${bad:-<empty>})"; fi
    done

    # positive control for the guard: a RESOLVABLE base must still classify and pass, or the
    # rejections above prove only that the script exits non-zero on everything.
    if "$REPO_ROOT/scripts/check-review.sh" --message-file "$tmp/m6" --base HEAD >/dev/null 2>&1; then
        echo "  ok: a resolvable base still classifies (positive control)"
    else echo "SELF-TEST FAIL: a resolvable base was rejected"; rc=1; fi

    [ "$rc" -eq 0 ] && echo "REVIEW-LINT-SELF-TEST: PASS" || echo "REVIEW-LINT-SELF-TEST: FAIL"
    exit "$rc"
fi

# Classify the change.
#
# FAIL CLOSED (#1219). Every step below used to degrade to "ordinary change" on failure, which is
# the worst possible default for a check whose job is to demand a review artifact: an unresolvable
# base silently waved design-class work through with `Review: none`, and nothing said so. Three
# distinct ways it happened, all measured:
#   - no --base at all               -> the `[ -n "$BASE" ]` guard skipped classification entirely;
#   - a 40-hex SHA that does not exist -> `git rev-parse --verify` returns 0 for a well-formed
#     object NAME, so the old existence check did not fire; `git diff` then failed into 2>/dev/null
#     and an empty diff classified as ordinary. `^{commit}` is what actually checks existence;
#   - any other diff failure          -> swallowed by the same 2>/dev/null.
# A base that cannot be resolved is not evidence of anything, least of all of innocence.
design=0
if [ -z "$BASE" ]; then
    echo "review-lint: no --base given, so the change cannot be classified." >&2
    echo "             Pass the PR's base branch, e.g. --base origin/main." >&2
    echo "REVIEW-LINT: FAIL"
    exit 2
fi
if ! git rev-parse --verify --quiet "${BASE}^{commit}" >/dev/null 2>&1; then
    echo "review-lint: base '$BASE' does not resolve to a commit in this checkout." >&2
    echo "             Refusing to classify: an unresolvable base is not 'not design-class'." >&2
    echo "REVIEW-LINT: FAIL"
    exit 2
fi
if ! diff_out=$(git diff --name-status "$BASE...HEAD" 2>&1); then
    echo "review-lint: 'git diff $BASE...HEAD' failed: $diff_out" >&2
    echo "REVIEW-LINT: FAIL"
    exit 2
fi
if printf '%s\n' "$diff_out" | is_design_class; then design=1; fi
[ "$design" -eq 1 ] && echo "review-lint: DESIGN-CLASS change (touches a decision site)" \
                    || echo "review-lint: ordinary change"

if [ -n "$MSG_FILE" ]; then
    [ -f "$MSG_FILE" ] || { echo "review-lint: no such message file: $MSG_FILE"; exit 2; }
    if lint_message "$(cat "$MSG_FILE")" "$design"; then
        echo "REVIEW-LINT: PASS"; exit 0
    fi
    echo "REVIEW-LINT: FAIL"; exit 1
fi

# No message to lint (local gate run): report the classification so the author sees it before push.
if [ "$design" -eq 1 ]; then
    echo "  note: this branch is design-class — its PR body will need a real Review: artifact."
fi
echo "REVIEW-LINT: PASS"
exit 0
