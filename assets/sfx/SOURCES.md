# SFX provenance

All files are mono 44.1 kHz Ogg Vorbis (oggenc q3), processed with ffmpeg. Everything here is CC0 — no attribution
required (given anyway).

## From Kenney "Impact Sounds" (CC0)

Source: https://kenney.nl/assets/impact-sounds (kenney_impact-sounds.zip, License.txt in the pack states Creative
Commons Zero).

| file              | built from                                                                                           |
| ----------------- | ---------------------------------------------------------------------------------------------------- |
| `dig_stone.ogg`   | `impactMining_000..003` — four hits padded to 0.22 s slots, concatenated into a ~0.9 s rhythmic loop |
| `dig_earth.ogg`   | `impactSoft_heavy_000..003`, same construction                                                       |
| `dig_wood.ogg`    | `impactWood_medium_000..003`, same construction                                                      |
| `break_stone.ogg` | `impactMining_004`                                                                                   |
| `break_earth.ogg` | `impactSoft_heavy_004`                                                                               |
| `break_wood.ogg`  | `impactWood_heavy_004`                                                                               |
| `place.ogg`       | `impactSoft_medium_000`                                                                              |
| `step_stone.ogg`  | `footstep_concrete_000`                                                                              |
| `step_earth.ogg`  | `footstep_grass_000`                                                                                 |
| `step_wood.ogg`   | `footstep_wood_000`                                                                                  |

## Synthesized (CC0 by construction)

Generated in-repo with ffmpeg `anoisesrc` noise + filters, tail crossfaded into the head for a silent loop seam (the
build recipe lives in the commit that added these files):

- `waterfall.ogg` — brown noise bandpassed 120–2400 Hz + a white-noise hiss layer, 7 s loop
- `ambient_wind.ogg` — pink noise lowpassed at 900 Hz with a 0.13 Hz swell, 14 s loop
- `ambient_water.ogg` — brown noise lowpassed at 420 Hz (muffled underwater bed), 12 s loop
- `ambient_cave.ogg` — sub-160 Hz brown drone + faint airy shimmer, 16 s loop
- `swim.ogg` — one swim stroke, 0.55 s one-shot, played on a distance cadence while swimming. Deliberately softer and
  rounder than `splash.ogg`: a mid-band noise swirl for the arm through the surface over a low body roll. Like
  `splash.ogg` it ends in silence, no seam crossfade.

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

- `level_up.ogg` — the ordinary level-up, 0.75 s one-shot: A5 into E6 a beat later, each a sine plus a quarter-strength
  octave for a bell-ish edge, exponential decays. Non-positional, so it needs no seam.

  ```sh
  ffmpeg -f lavfi -i "sine=f=880:d=0.75:r=44100" -f lavfi -i "sine=f=1760:d=0.75:r=44100" \
         -f lavfi -i "sine=f=1318.51:d=0.75:r=44100" -f lavfi -i "sine=f=2637.02:d=0.75:r=44100" \
    -filter_complex "[0:a]afade=t=in:st=0:d=0.008,afade=t=out:st=0.02:d=0.40:curve=exp,volume=0.9[n1];\
  [1:a]afade=t=in:st=0:d=0.008,afade=t=out:st=0.02:d=0.22:curve=exp,volume=0.25[n1h];\
  [2:a]adelay=140|140,afade=t=in:st=0.14:d=0.008,afade=t=out:st=0.16:d=0.55:curve=exp,volume=0.85[n2];\
  [3:a]adelay=140|140,afade=t=in:st=0.14:d=0.008,afade=t=out:st=0.16:d=0.30:curve=exp,volume=0.22[n2h];\
  [n1][n1h][n2][n2h]amix=inputs=4:normalize=0,aformat=sample_fmts=s16:channel_layouts=mono,\
  alimiter=level_in=1:level_out=0.9,afade=t=out:st=0.70:d=0.05" -ar 44100 -ac 1 level_up.wav
  oggenc -q 3 -o level_up.ogg level_up.wav
  ```

- `level_unlock.ogg` — the level-up that opens something, 1.30 s: the same construction escalated to a C major arpeggio
  (C6 E6 G6 C7) at 110 ms apart, the top note held twice as long over a brief high-passed noise shimmer. Deliberately
  the same tonic family as `level_up.ogg` — one event, two sizes.

  ```sh
  ffmpeg -f lavfi -i "sine=f=1046.50:d=1.30:r=44100" -f lavfi -i "sine=f=2093.00:d=1.30:r=44100" \
         -f lavfi -i "sine=f=1318.51:d=1.30:r=44100" -f lavfi -i "sine=f=2637.02:d=1.30:r=44100" \
         -f lavfi -i "sine=f=1567.98:d=1.30:r=44100" -f lavfi -i "sine=f=3135.96:d=1.30:r=44100" \
         -f lavfi -i "sine=f=2093.00:d=1.30:r=44100" -f lavfi -i "sine=f=4186.01:d=1.30:r=44100" \
         -f lavfi -i "anoisesrc=d=1.30:c=white:a=0.9:r=44100" \
    -filter_complex "[0:a]afade=t=in:st=0:d=0.006,afade=t=out:st=0.02:d=0.45:curve=exp,volume=0.75[a1];\
  [1:a]afade=t=in:st=0:d=0.006,afade=t=out:st=0.02:d=0.25:curve=exp,volume=0.20[a1h];\
  [2:a]adelay=110|110,afade=t=in:st=0.11:d=0.006,afade=t=out:st=0.13:d=0.45:curve=exp,volume=0.72[a2];\
  [3:a]adelay=110|110,afade=t=in:st=0.11:d=0.006,afade=t=out:st=0.13:d=0.25:curve=exp,volume=0.18[a2h];\
  [4:a]adelay=220|220,afade=t=in:st=0.22:d=0.006,afade=t=out:st=0.24:d=0.50:curve=exp,volume=0.72[a3];\
  [5:a]adelay=220|220,afade=t=in:st=0.22:d=0.006,afade=t=out:st=0.24:d=0.28:curve=exp,volume=0.18[a3h];\
  [6:a]adelay=330|330,afade=t=in:st=0.33:d=0.006,afade=t=out:st=0.35:d=0.85:curve=exp,volume=0.85[a4];\
  [7:a]adelay=330|330,afade=t=in:st=0.33:d=0.006,afade=t=out:st=0.35:d=0.45:curve=exp,volume=0.22[a4h];\
  [8:a]highpass=f=5000,adelay=330|330,afade=t=in:st=0.33:d=0.01,afade=t=out:st=0.35:d=0.55:curve=cub,volume=0.30[sh];\
  [a1][a1h][a2][a2h][a3][a3h][a4][a4h][sh]amix=inputs=9:normalize=0,\
  aformat=sample_fmts=s16:channel_layouts=mono,alimiter=level_in=1:level_out=0.92,\
  afade=t=out:st=1.24:d=0.06" -ar 44100 -ac 1 level_unlock.wav
  oggenc -q 3 -o level_unlock.ogg level_unlock.wav
  ```

- `splash.ogg` — water entry, 0.45 s one-shot. Two layers: a bright bandpassed white-noise impact decaying over 0.2 s,
  and a low brown-noise gurgle around 420 Hz decaying over 0.4 s. Unlike the loops above there is no seam crossfade — it
  ends in silence.

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
