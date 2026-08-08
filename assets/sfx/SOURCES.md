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
- `swim.ogg` — one swim stroke, 0.55 s one-shot, played on a distance
  cadence while swimming. Deliberately softer and rounder than
  `splash.ogg`: a mid-band noise swirl for the arm through the surface
  over a low body roll. Like `splash.ogg` it ends in silence, no seam
  crossfade.

  ```sh
  ffmpeg -f lavfi -i "anoisesrc=d=0.55:c=white:a=0.9:r=44100" \
         -f lavfi -i "anoisesrc=d=0.55:c=brown:a=1.0:r=44100" \
    -filter_complex "[0:a]bandpass=f=1400:width_type=o:w=2.4,afade=t=in:st=0:d=0.06:curve=qsin,afade=t=out:st=0.08:d=0.30:curve=cub,volume=0.7[sw];\
  [1:a]lowpass=f=520,afade=t=in:st=0:d=0.04,afade=t=out:st=0.05:d=0.45:curve=cub,volume=2.0[bo];\
  [sw][bo]amix=inputs=2:normalize=0,aformat=sample_fmts=s16:channel_layouts=mono,\
  alimiter=level_in=1:level_out=0.9,dynaudnorm=f=120:g=3:p=0.85,alimiter=level_out=0.8,\
  afade=t=out:st=0.50:d=0.05" -ar 44100 -ac 1 swim.wav
  oggenc -q 3 -o swim.ogg swim.wav
  ```

- `splash.ogg` — water entry, 0.45 s one-shot. Two layers: a bright
  bandpassed white-noise impact decaying over 0.2 s, and a low brown-noise
  gurgle around 420 Hz decaying over 0.4 s. Unlike the loops above there
  is no seam crossfade — it ends in silence.

  ```sh
  ffmpeg -f lavfi -i "anoisesrc=d=0.45:c=white:a=0.9:r=44100" \
         -f lavfi -i "anoisesrc=d=0.45:c=brown:a=1.0:r=44100" \
    -filter_complex "[0:a]highpass=f=900,lowpass=f=7000,afade=t=out:st=0:d=0.20:curve=cub,volume=0.9[hi];\
  [1:a]bandpass=f=420:width_type=o:w=2.2,afade=t=in:st=0:d=0.015,afade=t=out:st=0.03:d=0.40:curve=cub,volume=2.4[lo];\
  [hi][lo]amix=inputs=2:normalize=0,aformat=sample_fmts=s16:channel_layouts=mono,\
  alimiter=level_in=1:level_out=0.95,dynaudnorm=f=100:g=3:p=0.9,alimiter=level_out=0.9,\
  afade=t=out:st=0.41:d=0.04" -ar 44100 -ac 1 splash.wav
  oggenc -q 3 -o splash.ogg splash.wav
  ```
