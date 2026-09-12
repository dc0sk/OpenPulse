---
project: openpulsehf
doc: docs/dev/reviews/2026-08-26-1191-dst-station-removal.md
status: review
last_updated: 2026-08-26
---

# Adversarial review — removing `dst_station` from the CONACK, and cap 18 (#1191)

**Reviewer:** Fable · **Date:** 2026-08-26 · **Covers:** a wire-format design decision, reviewed
BEFORE implementation.

## Prompt

Sent with the apparatus: the measured budget from the prior review (CONREQ 224 / CONACK 244 at cap
12, 7 B headroom, cap 15 the ceiling without a trim); the maintainer's choice of cap 16 via removing
`dst_station` from the CONACK; and my own verification, with a positive control, that the daemon's
CONACK path reads `conreq_hash`, `station_id` and `kex_pubkey` and never `dst_station`.

Seven attacks requested, including: is the binding as strong without it (three specific scenarios —
simultaneous dials, replay from an earlier session, a CONACK re-presented to a different initiator);
does removing a signed field weaken the span; should the PQ CONACK match; re-derive the budget,
which I flagged as **arithmetic, not measurement**; what breaks; and is the trade right at all.

## Verdict

**Sound; proceed — with one addition the plan omitted.**

**The binding is strictly stronger without it**, verified at mechanism level rather than argued. The
hash gate runs *before* any teardown, so a foreign CONACK cannot kill a pending handshake. All three
scenarios die at that gate, and the replay case is the one where the hash is **strictly stronger
than a callsign echo**: a re-dial to the same peer produces the same `dst_station` but a different
`conreq_hash`. The decisive evidence that the field never had defensive value is the repo's own F2
test — the attacker fixture fills `dst_station` **correctly**, because it is attacker-controlled
content in a self-signed frame.

**Nothing binds to it implicitly.** `derive_ack_key` is pure ECDH over the two `kex_pubkey`s, so
shortening the CONACK's signed span affects no derived key. Type confusion stays closed by magic
plus `SigningDomain::ConAck`.

**Remove it from the PQ CONACK too.** `verify_pq_conack` binds by the same hash and never reads it;
there is no PQ KAT to re-record; and leaving it guarantees the eventual PQ wiring either carries a
known-dead field forever or pays a second wire break — the outcome that scoping PQ into #1147 was
meant to avoid.

**A correction to my framing that changed the decision.** Post-removal an extra id byte costs 2 on
the CONREQ but only **1** on the CONACK — so cap **18** costs only +4/+2 over 16 and covers a
compounded eight-character special-event call (`3DA0/VI110ACT/QRP`, 17). "The cap is a policy number
over an unbounded generator and the wire break is being paid regardless — decide it, don't default
it." **The maintainer chose 18.**

**The omission: a version bump.** The KAT's own failure message states the contract — an intentional
wire change must move the version byte, or a stale peer gets a garbled decode rather than a clean
rejection.

**Also found, filed separately:** `ConnectPeer` accepts `"*"`, which broadcasts a CONREQ every daemon
answers and then rejects every reply (#1203). And the wire spec is **already wrong on `main`** —
`session_id | 24`, a 241 B maximal CONREQ — predating the `session_id` → `u64` change.

## Applied — and where the maintainer overrode it

All of it, except the version bump. **The maintainer instead reset `WIRE_VERSION` to `0x01` and
froze it until 1.0**, on the grounds that nothing is deployed outside this project's test rigs, so
bumping per change is ceremony. That is a legitimate call and the reasoning holds; the cost the
review identified is real and is now recorded in the code and the spec rather than lost: two builds
from different points in the pre-1.0 window fail with a garbled decode, and the mitigation is
procedural — rebuild both ends in lockstep before any on-air session.

Budget gates fired as designed and their arithmetic was redone, not adjusted: **CONREQ 236,
CONACK 237**, matching the review's predicted numbers for cap 18. The stale spec numbers were swept,
and the spec's embedded vector — which had drifted from the code in both length and content — is now
taken verbatim from the test and asserted byte-identical.
