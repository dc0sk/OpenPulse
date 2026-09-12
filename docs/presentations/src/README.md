---
project: openpulsehf
doc: docs/presentations/src/README.md
status: archive
last_updated: 2026-08-26
---

# Presentation source

`../OpenPulseHF-Overview.odp` is **generated**, not hand-edited. Regenerate with:

```sh
cd docs/presentations/src
python3 gen.py        # English -> ../OpenPulseHF-Overview.odp
python3 gen.py de     # German  -> ../OpenPulseHF-Overview-DE.odp
```

`slides.py` and `slides_de.py` hold the text; `gen.py`, `diagrams.py` and both checks are SHARED, so
a layout fix applies to both languages and they cannot drift apart structurally. The diagrams carry
their own strings per language and are keyed by slide title in `gen.py` — rename a title and its
diagram silently detaches, which is why `check_titles` runs on every build.

- `slides.py` — all slide text and the project URL. Edit here.
- `diagrams.py` — the four native-ODF figures (evidence stack, fade bars, layer blocks, rate
  ladder) plus their styles. Every number in it is quoted from the repository, with the source
  named per diagram.
- `gen.py` — ODF emitter. Writes the `.odp` to the repo root path set at the bottom.
- `qr.txt` / `matrix.txt` are intermediates; they are not committed.

## Notes

**The QR is the maintainer's own image**, embedded as `Pictures/qr.png` from `qr-provided.png`. It
is stylised and points at the project site. It replaced a vector QR the generator used to draw from
`qrencode` output — a QR a room full of people will scan should be the one its owner validated,
not one regenerated from a URL string that can drift out of step with the slide text.

**Its payload has not been verified on this host.** The image is stylised (a coloured centre
element) which defeats naive matrix extraction, and no QR decoder is installed. If one ever is,
check the payload against `URL` in `slides.py` rather than assuming they agree.


## Editing by hand

Opening the `.odp` in LibreOffice and saving works fine — but the next `gen.py` run
overwrites it. Either keep edits in `slides.py`, or stop regenerating once hand-editing
starts.

## Two guards, and why there are two

`check_titles` (in `gen.py`) refuses a title over 48 characters. At 26 pt in a 25.2 cm frame a
longer one wraps, and the second line falls outside the header band and is clipped. **The render
check cannot see this** — it looks below the FOOTER, and a clipped title is cut near the top. Three
English titles were shipping clipped before this guard existed, including one with "FreeDV" cut in
half, and the render check had reported PASS over all of them.

German runs 15–30 % longer than English, so it hit the same limit harder: eleven German titles
needed shortening. Keep body lines under ~78 characters for the same reason.

## Checking it visually

`gen.py`'s bounds guard proves no SHAPE leaves the page. **That is not the same property as "the
slide is readable"** — a text box sits comfortably inside the page while its TEXT overflows the box,
and LibreOffice then silently clips it. Three slides shipped that way and only a render showed it.

LibreOffice is available as a flatpak, so render and check:

```sh
docs/presentations/src/render.sh          # all slides -> target/deckrender/out/*.png
DECK=docs/presentations/OpenPulseHF-Overview-DE.odp docs/presentations/src/render.sh   # German
docs/presentations/src/render.sh 18 31    # or just these
python3 docs/presentations/src/check_render.py target/deckrender/out
```

`check_render.py` looks for ink below the footer baseline, which is where clipped text lands. It
skips full-bleed dark slides (cover, closing) — measuring absolute ink there flagged the cover at
28 700 "overflow" pixels on the first attempt.

Two traps the tooling now guards against, both of which produced a wrong verdict first:
`render.sh` refuses to report on PNGs older than the deck (I read a stale image and concluded an
edit had not worked), and the splitter cuts each page at its own closing tag (the last slide made a
file LibreOffice would not load, so the check passed over 33 of 34 and said nothing).

## The bounds guard

`gen.py` refuses to write a deck whose shapes leave the 28.0 x 15.75 cm page. This is not
belt-and-braces: **there is no LibreOffice on the build host**, so nothing renders the file and an
overflow would first appear on a projector. Two of the four diagrams overflowed on their first
build and the guard is what found them. Sabotage-verified — pushing a diagram off-page makes
`gen.py` exit non-zero and name the slide.

It checks geometry only. "No shape leaves the page" is not "it looks right"; open the deck before
presenting it.
