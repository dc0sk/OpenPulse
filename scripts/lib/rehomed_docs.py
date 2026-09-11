#!/usr/bin/env python3
"""Re-homed doc / attribute lint (#1345).

Rustdoc attaches every consecutive outer doc line (and an outer attribute) to the next item, so a
diff can move a doc or an attribute onto the wrong item without touching either. Nothing in the
toolchain sees the contiguous form: `clippy::empty_line_after_doc_comments` catches only a BLANK
line between doc and item. This lint judges the NEW file, not the hunk shape, and flags the three
edits that re-home a doc:

  INS / MOD / ATTR  an item, struct field, enum variant, `use`, `extern crate` or macro
                    invocation added directly under an existing
                    non-blank `///` or an existing `#[...]`. The walk up skips added doc,
                    attribute and blank lines and blank `///` lines, and stops at an added code
                    line. INS is a pure insertion, MOD a modifying hunk, and ATTR means the first
                    line reached is an attribute.
  DEL               an item, field or variant deleted from between a doc or attribute and whatever
                    follows it, so the orphaned doc now heads the next thing.
  OVR               an item the diff did not otherwise change whose leading doc block lost its head,
                    when (a) what remains is non-empty and (b) the lost head lines appear nowhere else
                    in the new file. (b) excludes moves and repairs; (a) excludes deleting a doc
                    outright, which is not a re-homing.

A pure-insertion hunk is slid back up before it is judged (while the line above it equals its last
inserted line). Git anchors such hunks on duplicated trailing lines, and neighbouring struct and
enum members share exactly those lines (`acdb1a0d`).

Known limits, stated so they are not assumed covered:
- a steal whose stolen doc is entirely rewritten in the same hunk: the owner is left with no doc,
  which reads the same as deleting a doc outright;
- a re-homing created during a below-threshold rename, which shows as a whole-file add;
- macro-generated items; `#[doc = ...]` and `/** */` docs (there are none in this tree).

Design and its two review rounds: docs/dev/design/rehomed-docs-check.md.

Usage:  rehomed_docs.py BASE HEAD      # lint BASE..HEAD (both must name commits); exit 1 on a hit
        rehomed_docs.py --self-test    # plant every covered form in a scratch repo
"""
import os
import re
import subprocess
import sys
import tempfile

ITEM = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?((async|unsafe|const|extern(\s+\"[^\"]*\")?)\s+)*"
                  r"(fn|struct|enum|union|trait|impl|mod|type|const|static|macro_rules!|use|extern)\b")
FIELD = re.compile(r"^\s*(pub(\([^)]*\))?\s+)?[a-z_][A-Za-z0-9_]*\s*:(?!:)")
VARIANT = re.compile(r"^\s*[A-Z][A-Za-z0-9_]*\s*(\(|\{|,|=|$)")
DOC = re.compile(r"^\s*///(?!/)(.*)$")
ATTR = re.compile(r"^\s*#\[")
LEAD = re.compile(r"^\s*(///(?!/)|#\[)")
HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")

# A user's diff config must not change what this parser reads.
GIT_DIFF = ["-c", "diff.noprefix=false", "-c", "diff.mnemonicPrefix=false", "-c", "color.ui=never",
            "diff", "--no-ext-diff", "--no-color", "-U0", "-M"]


def git(repo, *args):
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, check=False)


def show(repo, rev, path):
    r = git(repo, "show", f"{rev}:{path}")
    return r.stdout.splitlines() if r.returncode == 0 else []


# A macro invocation takes an outer attribute like any item. `use` and `extern crate` are in ITEM for
# the same reason: `5e80f296` inserted a `use` directly under an existing `#[cfg]`, which took the cfg
# off the `use` below it. A doc above a macro invocation is a different matter — rustc's
# `unused_doc_comments` already catches that one.
MACRO_CALL = re.compile(r"^\s*[a-z_][A-Za-z0-9_]*!\s*[\[({]")


def is_start(line):
    if line.strip().startswith("//"):
        return False
    return bool(ITEM.match(line) or FIELD.match(line) or VARIANT.match(line) or MACRO_CALL.match(line))


def blank_doc(line):
    m = DOC.match(line)
    return bool(m) and m.group(1).strip() == ""


def nonblank_doc(line):
    m = DOC.match(line)
    return bool(m) and m.group(1).strip() != ""


def lead_block_above(src, i):
    """The doc and attribute lines directly above 0-based line `i`, in file order."""
    k, block = i - 1, []
    while k >= 0 and LEAD.match(src[k]):
        block.append(src[k])
        k -= 1
    return list(reversed(block))


def parse_diff(text):
    """[{old, new, hunks: [(old_start, old_cnt, new_start, new_cnt, body)]}], body lines keep +/-."""
    files, cur, oldpath = [], None, None
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        ln = lines[i]
        if ln.startswith("diff --git "):
            cur, oldpath = None, None
        elif ln.startswith("--- ") and cur is None:
            oldpath = ln[6:] if ln.startswith("--- a/") else None
        elif ln.startswith("+++ ") and cur is None:
            cur = {"old": oldpath, "new": ln[6:] if ln.startswith("+++ b/") else None, "hunks": []}
            files.append(cur)
        else:
            m = HUNK.match(ln)
            if m and cur is not None:
                os_ = int(m.group(1))
                oc = int(m.group(2)) if m.group(2) is not None else 1
                ns = int(m.group(3))
                nc = int(m.group(4)) if m.group(4) is not None else 1
                body, j, need_m, need_p = [], i + 1, oc, nc
                # Read exactly the counted lines, so a removed line that itself starts with "-- " or
                # "++ " cannot be mistaken for a file header.
                while j < len(lines) and (need_m or need_p):
                    b = lines[j]
                    if b.startswith("\\"):
                        j += 1
                        continue
                    if b.startswith("-") and need_m:
                        need_m -= 1
                    elif b.startswith("+") and need_p:
                        need_p -= 1
                    else:
                        break
                    body.append(b)
                    j += 1
                cur["hunks"].append((os_, oc, ns, nc, body))
                i = j
                continue
        i += 1
    return files


def owner_of(old, stolen):
    """In the old file, the item the stolen line documented: walk down from it past docs and attributes."""
    target = stolen.strip()
    for k, line in enumerate(old):
        if line.strip() == target:
            j = k
            while j < len(old) and LEAD.match(old[j]):
                j += 1
            return old[j].strip() if j < len(old) else "?"
    return "?"


def lint_file(repo, base, head, f, normalise=True):
    src = show(repo, head, f["new"])
    old = show(repo, base, f["old"]) if f["old"] else []
    plus, minus, items, dels = set(), set(), [], []
    for (os_, oc, ns, nc, body) in f["hunks"]:
        adds = [b[1:] for b in body if b.startswith("+")]
        rems = [b[1:] for b in body if b.startswith("-")]
        if oc == 0 and adds:
            if normalise:
                while ns >= 2 and ns - 2 < len(src) and src[ns - 2] == adds[-1]:
                    ns -= 1
                    adds = [src[ns - 1]] + adds[:-1]
            for k, a in enumerate(adds):
                plus.add(ns + k)
                if is_start(a):
                    items.append((ns + k, False, True))
        else:
            rewrite = any(is_start(r) for r in rems)
            minus.update(range(os_, os_ + oc))
            for k, a in enumerate(adds):
                plus.add(ns + k)
                if is_start(a):
                    items.append((ns + k, rewrite, False))
            if nc == 0 and rewrite and ns >= 1:
                dels.append((ns, next(r for r in rems if is_start(r)).strip()))
    path = f["new"]
    hits = []
    for (n, rewrite, pure) in items:
        if rewrite:
            continue
        # Only doc, attribute and blank lines can sit between a doc and the item it heads: an added CODE
        # line stops the walk. Without that, a start-shaped line deep inside a rewritten block (a tail
        # `Ok(x)` matches the variant pattern) walks all the way up to an attribute that still belongs
        # to the block's first line (`6790d298`, a false positive the full replay found).
        k = n - 1
        while k >= 1 and ((k in plus and (LEAD.match(src[k - 1]) or not src[k - 1].strip()))
                          or blank_doc(src[k - 1])):
            k -= 1
        if k >= 1 and (nonblank_doc(src[k - 1]) or ATTR.match(src[k - 1])):
            kind = "ATTR" if ATTR.match(src[k - 1]) else ("INS" if pure else "MOD")
            hits.append((kind, path, n, src[k - 1].strip(), src[n - 1].strip(), owner_of(old, src[k - 1]), k))
    for (m_, deleted) in dels:
        before = src[m_ - 1] if 1 <= m_ <= len(src) else ""
        after = src[m_] if m_ < len(src) else ""
        if (nonblank_doc(before) or ATTR.match(before)) and (LEAD.match(after) or is_start(after)):
            hits.append(("DEL", path, m_, before.strip(), after.strip(), deleted, m_))
    if old:
        old_idx = {}
        for oi, s in enumerate(old):
            if ITEM.match(s) and (oi + 1) not in minus:
                old_idx.setdefault(s, []).append(oi)
        new_lines = {x.strip() for x in src}
        used = set()
        for ni, s in enumerate(src):
            if (ni + 1) in plus or not ITEM.match(s) or s not in old_idx:
                continue
            cands = [oi for oi in old_idx[s] if oi not in used]
            if not cands:
                continue
            oi = cands[0]
            used.add(oi)
            ob = [x.strip() for x in lead_block_above(old, oi)]
            nb = [x.strip() for x in lead_block_above(src, ni)]
            if nb and len(nb) < len(ob) and ob[len(ob) - len(nb):] == nb:
                lost = [x for x in ob[:len(ob) - len(nb)] if nonblank_doc(x)]
                if lost and not any(x in new_lines for x in lost):
                    hits.append(("OVR", path, ni + 1, lost[0], s.strip(), s.strip(), ni + 1))
    return hits


def lint(repo, base, head, normalise=True):
    d = git(repo, *GIT_DIFF, base, head, "--", "*.rs")
    if d.returncode != 0:
        raise RuntimeError(f"git diff {base} {head} failed: {d.stderr.strip()}")
    hits, seen = [], set()
    for f in parse_diff(d.stdout):
        if not f["new"]:
            continue
        # Topmost item first, so the one kept per stolen line is the item that now carries it.
        for h in sorted(lint_file(repo, base, head, f, normalise), key=lambda h: (h[2], h[0])):
            key = (h[0] == "OVR", h[1], h[6])
            if key not in seen:
                seen.add(key)
                hits.append(h)
    return hits


def report(hits):
    for kind, path, line, stolen, now_on, owner, _ in hits:
        print(f"  {kind:4} {path}:{line}")
        print(f"       doc/attr: {stolen}")
        print(f"       now on:   {now_on}")
        print(f"       owner:    {owner}")


# ---- self-test ---------------------------------------------------------------------------------
# One table for the covered forms: (name, expected kinds or None for PASS, old text, new text).
FIXTURES = [
    ("F1 item deleted between two docs", {"DEL"},
     "/// Doc of a.\nfn a() {}\n/// Doc of b.\nfn b() {}\n",
     "/// Doc of a.\n/// Doc of b.\nfn b() {}\n"),
    ("F1b field deleted between two field docs", {"DEL"},
     "struct S {\n    /// Doc of x.\n    x: u8,\n    /// Doc of y.\n    y: u8,\n}\n",
     "struct S {\n    /// Doc of x.\n    /// Doc of y.\n    y: u8,\n}\n"),
    ("F2 reflowed doc plus an inserted item", {"MOD"},
     "/// Summary of a.\n///\n/// Detail that is\n/// long.\nfn a() {}\n",
     "/// Summary of a.\n///\n/// Detail that is long.\nfn b() {}\n\nfn a() {}\n"),
    ("F3 bare attribute, no doc", {"ATTR"},
     "#[cfg(test)]\nfn a() {}\n",
     "#[cfg(test)]\nfn b() {}\n\nfn a() {}\n"),
    ("F4 plain insertion under a doc", {"INS"},
     "/// Doc of a.\nfn a() {}\n",
     "/// Doc of a.\nfn b() {}\n\nfn a() {}\n"),
    ("F5 insertion under a doc with an attribute between", {"ATTR"},
     "/// Doc of a.\n#[inline]\nfn a() {}\n",
     "/// Doc of a.\n#[inline]\nfn b() {}\n\nfn a() {}\n"),
    ("F6 insertion after a NEW blank line under a doc", {"INS"},
     "/// Doc of a.\nfn a() {}\n",
     "/// Doc of a.\n\nfn b() {}\n\nfn a() {}\n"),
    ("F7 summary overwritten, remainder non-empty", {"OVR"},
     "/// Summary of a.\n///\n/// Detail of a.\nfn a() {}\n",
     "/// Doc of b.\nfn b() {}\n///\n/// Detail of a.\nfn a() {}\n"),
    ("F8 field inserted under the previous field's doc", {"INS"},
     "struct S {\n    /// Doc of x.\n    x: u8,\n}\n",
     "struct S {\n    /// Doc of x.\n    w: u8,\n    x: u8,\n}\n"),
    ("F9 item under a cfg above a struct-literal field", {"ATTR"},
     "fn f() -> S {\n    S {\n        #[cfg(feature = \"gpu\")]\n        gpu: None,\n    }\n}\n",
     "fn f() -> S {\n    S {\n        #[cfg(feature = \"gpu\")]\n        other: 1,\n        gpu: None,\n    }\n}\n"),
    ("F10 use inserted under a pre-existing cfg, taking it (5e80f296)", {"ATTR"},
     "#[cfg(unix)]\nuse a::b;\n#[cfg(unix)]\nuse c::d;\n",
     "#[cfg(unix)]\nuse a::b;\n#[cfg(unix)]\nuse e::f;\nuse c::d;\n"),
    ("F11 macro invocation inserted under a pre-existing cfg", {"ATTR"},
     "#[cfg(unix)]\nlazy_static! { static ref A: u8 = 1; }\n",
     "#[cfg(unix)]\nlazy_static! { static ref B: u8 = 2; }\nlazy_static! { static ref A: u8 = 1; }\n"),
    ("P1 doc deleted together with its item", None,
     "fn z() {}\n\n/// Doc of a.\nfn a() {}\n\nfn c() {}\n",
     "fn z() {}\n\nfn c() {}\n"),
    ("P2 signature rewrite under an unchanged doc", None,
     "/// Doc of a.\nfn a(x: u8) {}\n",
     "/// Doc of a.\nfn a(x: u16) {}\n"),
    ("P3 attribute line added on its own", None,
     "/// Doc of a.\nfn a() {}\n",
     "/// Doc of a.\n#[inline]\nfn a() {}\n"),
    ("P4 paragraph appended to a doc", None,
     "/// Doc of a.\nfn a() {}\n",
     "/// Doc of a.\n///\n/// More about a.\nfn a() {}\n"),
    ("P5 last item deleted with its doc", None,
     "fn z() {}\n\n/// Doc of a.\nfn a() {}\n",
     "fn z() {}\n"),
    ("P6 insertion after an EXISTING blank line", None,
     "/// Doc of a.\nfn a() {}\n\nfn c() {}\n",
     "/// Doc of a.\nfn a() {}\n\nfn b() {}\n\nfn c() {}\n"),
    ("P7 insertion after a //! inner doc", None,
     "//! Crate doc.\nfn a() {}\n",
     "//! Crate doc.\nfn b() {}\nfn a() {}\n"),
    # P9 is the real acdb1a0d region: git anchors the new variant so that its last inserted line
    # equals the doc line above the insertion point, which puts `enabled: bool,` under that doc.
    ("P9 slid insertion (acdb1a0d, -12/+1 lines of real context)", None,
     '        /// RMS threshold (0.0–1.0); raise above the band noise floor.\n        threshold: f32,\n    },\n    /// Enable/disable CE-SSB TX envelope conditioning (multicarrier modes only).\n    SetCessb {\n        /// true to enable, false to disable.\n        #[arg(action = clap::ArgAction::Set)]\n        enabled: bool,\n    },\n    /// Enable/disable the receiver-side automatic notch (removes out-of-band CW interference).\n    SetNotch {\n        /// true to enable, false to disable.\n        #[arg(action = clap::ArgAction::Set)]\n',
     '        /// RMS threshold (0.0–1.0); raise above the band noise floor.\n        threshold: f32,\n    },\n    /// Enable/disable CE-SSB TX envelope conditioning (multicarrier modes only).\n    SetCessb {\n        /// true to enable, false to disable.\n        #[arg(action = clap::ArgAction::Set)]\n        enabled: bool,\n    },\n    /// Enable/disable the receiver-side automatic notch (removes out-of-band CW interference).\n    SetNotch {\n        /// true to enable, false to disable.\n        #[arg(action = clap::ArgAction::Set)]\n        enabled: bool,\n    },\n    /// Enable/disable the automatic ADIF logbook (one record per connect→disconnect).\n    SetLogbook {\n        /// true to enable, false to disable.\n        #[arg(action = clap::ArgAction::Set)]\n'),
    ("P12 rewritten block under an expression attribute, tail expression start-shaped (6790d298)", None,
     "fn f(m: M) -> R {\n    match m {\n        #[allow(deprecated)]\n        M::A => old_a(),\n        M::B => old_b(),\n    }\n}\n",
     "fn f(m: M) -> R {\n    let w = match m {\n        #[allow(deprecated)]\n        M::A => {\n            old_a()?;\n            vec![]\n        }\n"
     "        M::B => {\n            old_b()?;\n            vec![]\n        }\n    };\n\n    Ok(w)\n}\n"),
    ("P10 whole doc deleted from an unchanged item", None,
     "/// Doc of a.\nfn a() {}\n",
     "fn a() {}\n"),
    ("P11 doc moved back to its owner (the repair shape)", None,
     "/// Doc of a.\n/// Doc of b.\nfn b() {}\n\nfn a() {}\n",
     "/// Doc of b.\nfn b() {}\n\n/// Doc of a.\nfn a() {}\n"),
]


def self_test():
    failures = []
    with tempfile.TemporaryDirectory(prefix="rehomed-docs-selftest-") as repo:
        env_git = ["-c", "user.name=self-test", "-c", "user.email=self-test@localhost",
                   "-c", "commit.gpgsign=false"]
        git(repo, "init", "-q")
        for idx, (name, expect, old, new) in enumerate(FIXTURES):
            fname = f"f{idx:02d}.rs"
            with open(os.path.join(repo, fname), "w", encoding="utf-8") as fh:
                fh.write(old)
            git(repo, "add", fname)
            git(repo, *env_git, "commit", "-q", "-m", f"old {name}")
            base = git(repo, "rev-parse", "HEAD").stdout.strip()
            with open(os.path.join(repo, fname), "w", encoding="utf-8") as fh:
                fh.write(new)
            git(repo, "add", fname)
            git(repo, *env_git, "commit", "-q", "-m", f"new {name}")
            head = git(repo, "rev-parse", "HEAD").stdout.strip()
            kinds = {h[0] for h in lint(repo, base, head)}
            ok = (not kinds) if expect is None else bool(kinds & expect)
            verdict = "ok  " if ok else "FAIL"
            print(f"  {verdict} {name}: expected {sorted(expect) if expect else 'no hit'}, got {sorted(kinds) or 'no hit'}")
            if not ok:
                failures.append(name)
            if name.startswith("P9"):
                # The slide fixture proves nothing unless git really slid it: without normalisation the
                # same diff must produce the false hit that normalisation removes.
                raw = {h[0] for h in lint(repo, base, head, normalise=False)}
                slid = bool(raw)
                print(f"  {'ok  ' if slid else 'FAIL'} P9 discriminates: without slide normalisation it "
                      f"{'flags ' + str(sorted(raw)) if slid else 'does NOT flag, so the fixture did not slide'}")
                if not slid:
                    failures.append("P9 does not discriminate")
    if failures:
        print(f"SELF-TEST: FAIL ({len(failures)}): {', '.join(failures)}")
        return 1
    print(f"SELF-TEST: PASS ({len(FIXTURES)} fixtures, slide fixture discriminates)")
    return 0


def main(argv):
    if argv[1:2] == ["--self-test"]:
        return self_test()
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[-2], file=sys.stderr)
        return 2
    base, head = argv[1], argv[2]
    try:
        hits = lint(".", base, head)
    except RuntimeError as e:
        print(f"rehomed-docs: {e}", file=sys.stderr)
        return 2
    if hits:
        print(f"re-homed docs/attributes introduced by {base}..{head}: {len(hits)}")
        report(hits)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
