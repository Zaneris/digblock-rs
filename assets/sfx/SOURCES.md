# SFX provenance

All files are mono 44.1 kHz Ogg Vorbis (oggenc q3), processed with ffmpeg.
Everything here is CC0 — no attribution required (given anyway).

## From Kenney "Impact Sounds" (CC0)

Source: https://kenney.nl/assets/impact-sounds (kenney_impact-sounds.zip,
License.txt in the pack states Creative Commons Zero).

| file | built from |
|---|---|
| `dig_stone.ogg` | `impactMining_000..003` — four hits padded to 0.22 s slots, concatenated into a ~0.9 s rhythmic loop |
| `dig_earth.ogg` | `impactSoft_heavy_000..003`, same construction |
| `dig_wood.ogg` | `impactWood_medium_000..003`, same construction |
| `break_stone.ogg` | `impactMining_004` |
| `break_earth.ogg` | `impactSoft_heavy_004` |
| `break_wood.ogg` | `impactWood_heavy_004` |
| `place.ogg` | `impactSoft_medium_000` |
| `step_stone.ogg` | `footstep_concrete_000` |
| `step_earth.ogg` | `footstep_grass_000` |
| `step_wood.ogg` | `footstep_wood_000` |

## Synthesized (CC0 by construction)

Generated in-repo with ffmpeg `anoisesrc` noise + filters, tail
crossfaded into the head for a silent loop seam (the build recipe lives
in the commit that added these files):

- `waterfall.ogg` — brown noise bandpassed 120–2400 Hz + a white-noise hiss layer, 7 s loop
- `ambient_wind.ogg` — pink noise lowpassed at 900 Hz with a 0.13 Hz swell, 14 s loop
- `ambient_water.ogg` — brown noise lowpassed at 420 Hz (muffled underwater bed), 12 s loop
- `ambient_cave.ogg` — sub-160 Hz brown drone + faint airy shimmer, 16 s loop
