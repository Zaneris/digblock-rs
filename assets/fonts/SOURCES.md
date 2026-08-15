# Font provenance

Both faces are SIL Open Font License 1.1. The license text ships next to the fonts (`OFL-Inter.txt`,
`OFL-JetBrainsMono.txt`) because OFL requires it to travel with the font data, and these are `include_bytes!`d into the
binary.

Both are shipped as **static, subset** TTFs. Upstream ships variable fonts; `fontdue` does not apply variations, so a
variable file would silently render its default master while carrying every other master's outlines. We pin the instance
explicitly and then subset — see the commands below.

| file | face | upstream | version |
|---|---|---|---|
| `ui.ttf` | Inter Regular | [google/fonts `ofl/inter`](https://github.com/google/fonts/tree/main/ofl/inter) @ `2d85e20` | 4.001 (`git-66647c0bb`) |
| `mono.ttf` | JetBrains Mono Regular | [google/fonts `ofl/jetbrainsmono`](https://github.com/google/fonts/tree/main/ofl/jetbrainsmono) @ `2d85e20` | 2.211 |

`ui.ttf` draws all player-facing text. `mono.ttf` draws only the debug overlay and the spike log, which align columns
with printf padding and therefore need a fixed advance.

## Repertoire

Latin-1 Supplement and Latin Extended-A, so European players can type their own names and words in chat:

```
U+0020-007E  Basic Latin (printable)
U+00A0-00FF  Latin-1 Supplement
U+0100-017F  Latin Extended-A
U+2013-2014  en dash, em dash
U+2018-201D  curly quotes
U+20AC       euro sign
```

Known gaps, all deliberate — `font::REPERTOIRE` excludes them and `is_typeable` rejects them, so they can never reach
the renderer:

- `U+00AD` soft hyphen — invisible, and neither face draws it.
- `U+0149` ŉ — deprecated, absent from Inter.
- `U+0132`/`U+0133` Ĳ/ĳ — absent from JetBrains Mono. Present in Inter, so chat renders them fine; only the debug
  overlay could not, and that draws engine-generated ASCII exclusively.

`font::face` has a test asserting the UI face covers every codepoint in `REPERTOIRE` and the mono face covers printable
ASCII. Swapping in a different face fails that test rather than silently rendering `?`.

## Regeneration

Requires `fonttools` (any recent version; 4.63.0 was used). Use a virtualenv — Homebrew's Python is externally managed:

```sh
python3 -m venv /tmp/fontenv && /tmp/fontenv/bin/pip install fonttools
BIN=/tmp/fontenv/bin
UNI='U+0020-007E,U+00A0-00FF,U+0100-017F,U+2013-2014,U+2018-201D,U+20AC'

# Inter: pin weight 400 and optical size 14 (the default, tuned for small text).
curl -sSL -o Inter-var.ttf \
  'https://raw.githubusercontent.com/google/fonts/main/ofl/inter/Inter%5Bopsz%2Cwght%5D.ttf'
$BIN/fonttools varLib.instancer -q Inter-var.ttf wght=400 opsz=14 -o Inter-400.ttf
$BIN/pyftsubset Inter-400.ttf --unicodes="$UNI" --layout-features='' --no-hinting \
  --desubroutinize --drop-tables+=GSUB,GPOS --output-file=assets/fonts/ui.ttf

# JetBrains Mono: pin weight 400.
curl -sSL -o JBM-var.ttf \
  'https://raw.githubusercontent.com/google/fonts/main/ofl/jetbrainsmono/JetBrainsMono%5Bwght%5D.ttf'
$BIN/fonttools varLib.instancer -q JBM-var.ttf wght=400 -o JBM-400.ttf
$BIN/pyftsubset JBM-400.ttf --unicodes="$UNI" --layout-features='' --no-hinting \
  --desubroutinize --drop-tables+=GSUB,GPOS --output-file=assets/fonts/mono.ttf
```

Subsetting is not cosmetic. `fontdue::Font::from_bytes` eagerly outlines every glyph reachable from `cmap`, so the stock
876 KB Inter (2933 glyphs) would cost tens of milliseconds and several megabytes of heap at startup — on the wasm main
thread, for both faces. Subset, each face is ~22 KB and ~360 glyphs. `digblock-assets` has a size-guard test that fails
if an unsubset face is committed.

## Swapping a face

Drop the replacement in as `ui.ttf` or `mono.ttf` (subset with the commands above) and update the table here. Nothing
else changes — sizes, metrics and line height are all read from the file at runtime. The tests that will complain if the
new face is unsuitable: repertoire coverage, the size guard in `digblock-assets`, and `debug_panel_rows_dont_overlap` (a
mono face with a very tall line box).
