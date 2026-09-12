#!/usr/bin/env bash
# Traceability-ledger ordering lint — the file says "Newest first"; this makes that true.
#
# WHY THIS EXISTS. `docs/dev/project/traceability.md` declared newest-first in its own header and had
# silently drifted into TWO regimes: newest-first from the top, then an oldest-first tail, because
# entries were historically appended at the END and at some point people began prepending. Twelve
# ordering breaks accumulated across 359 entries, and the cost was not cosmetic — the four most
# recent entries sat past line 10 300 while material six weeks older sat above them, so the newest
# work was not where any reader looks. Nothing caught it because the convention lived only in prose.
#
# WHAT IT CHECKS
#   1. every `## ` entry heading carries a date (else the ordering is undefined, not merely wrong);
#   2. dates are non-increasing top to bottom.
#
# WHAT IT DELIBERATELY DOES NOT CHECK. Heading FORM. Eight legacy entries carry the date trailing
# (`## Some title (2026-07-29)`) rather than leading. Both forms are accepted, because rewriting a
# heading changes its Markdown anchor and would break any existing link to it — a real cost for a
# cosmetic gain. The trailing-form entries are reported as INFO so the count cannot grow unnoticed.
#
# Usage:  scripts/check-ledger-order.sh [FILE]
#         scripts/check-ledger-order.sh --self-test
set -u
REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$REPO_ROOT" || exit 2

check_file() {
    python3 - "$1" <<'PY'
import re, sys
path = sys.argv[1]
try:
    lines = open(path, encoding="utf-8").read().split("\n")
except OSError as e:
    print(f"cannot read {path}: {e}"); sys.exit(2)

LEAD = re.compile(r"^## (\d{4}-\d{2}-\d{2})")
ANY  = re.compile(r"(\d{4}-\d{2}-\d{2})")

entries, undated, trailing = [], [], []
for n, l in enumerate(lines, 1):
    if not l.startswith("## "):
        continue
    m = LEAD.match(l)
    if m:
        entries.append((n, m.group(1), l))
        continue
    m = ANY.search(l)
    if m:
        entries.append((n, m.group(1), l))
        trailing.append((n, l))
    else:
        undated.append((n, l))

fail = False
if undated:
    fail = True
    print(f"FAIL — {len(undated)} entry heading(s) carry no date, so their position is undefined:")
    for n, l in undated:
        print(f"  line {n}: {l[:100]}")

breaks = [(entries[i][0], entries[i-1][1], entries[i][1], entries[i][2])
          for i in range(1, len(entries)) if entries[i][1] > entries[i-1][1]]
if breaks:
    fail = True
    print(f"FAIL — {len(breaks)} entry(ies) are NEWER than the entry above them (file is newest-first):")
    for n, prev, cur, l in breaks:
        print(f"  line {n}: {cur} follows {prev}  |  {l[:80]}")
    print("  Fix: move the entry to its place by date. New entries go at the TOP, under the header.")

if trailing:
    print(f"INFO — {len(trailing)} heading(s) carry the date trailing rather than leading; accepted "
          f"(rewriting them would break their anchors), but prefer `## YYYY-MM-DD — title` for new ones.")

if not fail:
    print(f"ledger order: {len(entries)} entries, newest {entries[0][1]}, oldest {entries[-1][1]}")
sys.exit(1 if fail else 0)
PY
}

LEDGER="docs/dev/project/traceability.md"

if [ "${1:-}" = "--self-test" ]; then
    # A check nobody has watched FAIL is the self-consistent checker it exists to prevent.
    tmp=$(mktemp) || exit 2
    trap 'rm -f "$tmp"' EXIT

    # 1. an out-of-order pair must be rejected.
    #
    # The fixture is synthetic on purpose. This probe used to swap the real ledger's first two
    # entries, which stops discriminating the moment those two share a date: swapping equal dates
    # leaves the sequence non-increasing, the check correctly passes, and the probe then reports
    # "the check cannot detect disorder" about a checker that is fine. That fired on 2026-09-12,
    # when three entries landed on one day. A fixture whose planted defect depends on what the
    # real file happens to contain is not a fixture.
    printf '## 2026-01-01 older, wrongly first\n\nbody\n\n## 2026-02-02 newer\n\nbody\n' > "$tmp"
    if check_file "$tmp" > /dev/null 2>&1; then
        echo "SELF-TEST: FAIL - an out-of-order pair was ACCEPTED; the check cannot detect disorder"; exit 1
    fi

    # 1b. the same two entries in the right order must PASS, or probe 1 shows only that the check
    # rejects everything.
    printf '## 2026-02-02 newer\n\nbody\n\n## 2026-01-01 older\n\nbody\n' > "$tmp"
    if ! check_file "$tmp" > /dev/null 2>&1; then
        echo "SELF-TEST: FAIL - a correctly ordered pair was REJECTED; the check fails everything"; exit 1
    fi

    # 2. a dateless heading must be rejected
    { printf '## a heading with no date at all\n\nbody\n'; } > "$tmp"
    if check_file "$tmp" > /dev/null 2>&1; then
        echo "SELF-TEST: FAIL — a dateless heading was ACCEPTED; ordering would be undefined"; exit 1
    fi

    # 3. and the real file must PASS, or the check is unusable
    if ! check_file "$LEDGER" > /dev/null 2>&1; then
        echo "SELF-TEST: FAIL — the real ledger does not pass; fix it before wiring this in"; exit 1
    fi
    echo "SELF-TEST: PASS — out-of-order pair rejected, ordered pair accepted, dateless heading rejected, real ledger accepted"
    exit 0
fi

target="${1:-$LEDGER}"
if check_file "$target"; then
    echo "LEDGER-ORDER: PASS"
else
    echo "LEDGER-ORDER: FAIL"
    exit 1
fi
