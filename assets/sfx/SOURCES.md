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
build recipe lives in the commit that added these files). The two `siphon_*` recipes and `ward_close.ogg` pin their
noise seeds and the Ogg serial, so re-running one reproduces the committed file byte for byte and a recipe that has
drifted from its output says so; the older entries pin neither, and reproduce the sound rather than the bytes.

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

- `siphon_draw.ogg` — the Essence Siphon working a vein, 2.4 s loop, played on the hold voice in place of the dig loop.
  A resonant A drone (A2/A3/E4/A4/E5) with a second partial 1.25 Hz off the A3 so the body beats three times a loop, two
  inharmonic partials for a crystalline edge, a tremolo'd shimmer above 4.5 kHz for the motes, and a narrow noise band
  for airflow.

  Every frequency here is an exact multiple of 1/2.4 Hz, so the tonal half repeats with the loop and the tail crossfade
  is transparent rather than phase-cancelling — that is what the odd-looking partials are for, and the reason the loop
  length cannot be retuned without re-deriving them. The crossfade still earns its keep on the noise layers and on the
  filter/limiter state, which have no such period.

  A caution for anyone re-cutting this: the seam has to be judged on CURVATURE, not on `|s[0] - s[-1]|`. This waveform
  has steep legitimate slopes, so a sample-delta test calls a clean loop a click; the second difference at the wrap,
  compared against the local distribution either side of it, is the test that answers.

  ```sh
  ffmpeg -y -f lavfi -i "sine=f=110:d=2.8:r=44100" -f lavfi -i "sine=f=220:d=2.8:r=44100" \
         -f lavfi -i "sine=f=221.25:d=2.8:r=44100" -f lavfi -i "sine=f=330:d=2.8:r=44100" \
         -f lavfi -i "sine=f=440:d=2.8:r=44100" -f lavfi -i "sine=f=660:d=2.8:r=44100" \
         -f lavfi -i "sine=f=512.5:d=2.8:r=44100" -f lavfi -i "sine=f=1030:d=2.8:r=44100" \
         -f lavfi -i "anoisesrc=d=2.8:c=white:a=0.9:r=44100:seed=1071" \
         -f lavfi -i "anoisesrc=d=2.8:c=brown:a=1.0:r=44100:seed=2213" \
    -filter_complex "[0:a]volume=0.20[s0];[1:a]volume=0.34[s1];[2:a]volume=0.32[s2];\
  [3:a]volume=0.16[s3];[4:a]volume=0.11[s4];[5:a]volume=0.06[s5];\
  [6:a]volume=0.075[s6];[7:a]volume=0.045[s7];\
  [8:a]highpass=f=4500,tremolo=f=7.5:d=0.9,volume=0.10[sh];\
  [9:a]bandpass=f=900:width_type=o:w=1.2,volume=0.07[ru];\
  [s0][s1][s2][s3][s4][s5][s6][s7][sh][ru]amix=inputs=10:normalize=0,\
  highpass=f=60,tremolo=f=0.833333:d=0.18,alimiter=level_in=1:level_out=0.9,asplit=2[a][b];\
  [a]atrim=0:2.4,asetpts=PTS-STARTPTS,afade=t=in:st=0:d=0.4:curve=qsin[body];\
  [b]atrim=2.4:2.8,asetpts=PTS-STARTPTS,afade=t=out:st=0:d=0.4:curve=qsin[tail];\
  [body][tail]amix=inputs=2:normalize=0,volume=2.6,\
  aformat=sample_fmts=s16:channel_layouts=mono" -ar 44100 -ac 1 siphon_draw.wav
  oggenc -q 3 --serial 1 -o siphon_draw.ogg siphon_draw.wav
  ```

- `siphon_take.ogg` — the draw landing, 0.72 s one-shot. A bright A6 glint with a quarter-strength octave and an airy
  puff, settling 100 ms later onto E5 — which is a partial the loop was already sounding, so the resonance reads as
  collapsing into the tool. It FALLS, where both level cues rise: that is what keeps taking a seam from reading as
  gaining a level, since shape carries the distinction far better than key would. Ends in silence, no seam.

  The two decays are deliberately different curves. `exp` is very steep — inaudible in a fraction of its stated duration
  — which is right for the glint and wrong for the note underneath it, so the settle and the low weight take `cub` and
  actually ring.

  ```sh
  ffmpeg -y -f lavfi -i "sine=f=1760:d=0.62:r=44100" -f lavfi -i "sine=f=3520:d=0.62:r=44100" \
         -f lavfi -i "sine=f=659.26:d=0.62:r=44100" -f lavfi -i "sine=f=1318.51:d=0.62:r=44100" \
         -f lavfi -i "sine=f=110:d=0.62:r=44100" -f lavfi -i "anoisesrc=d=0.62:c=white:a=0.9:r=44100:seed=4409" \
    -filter_complex "[0:a]afade=t=in:st=0:d=0.006,afade=t=out:st=0.015:d=0.30:curve=exp,volume=0.75[g1];\
  [1:a]afade=t=in:st=0:d=0.006,afade=t=out:st=0.015:d=0.16:curve=exp,volume=0.18[g1h];\
  [2:a]adelay=100|100,afade=t=in:st=0.10:d=0.006,afade=t=out:st=0.115:d=0.55:curve=cub,volume=0.80[g2];\
  [3:a]adelay=100|100,afade=t=in:st=0.10:d=0.006,afade=t=out:st=0.115:d=0.30:curve=cub,volume=0.20[g2h];\
  [4:a]adelay=100|100,afade=t=in:st=0.10:d=0.02,afade=t=out:st=0.13:d=0.45:curve=cub,volume=0.30[lo];\
  [5:a]highpass=f=3000,afade=t=in:st=0:d=0.03:curve=qsin,afade=t=out:st=0.05:d=0.22:curve=cub,volume=0.22[air];\
  [g1][g1h][g2][g2h][lo][air]amix=inputs=6:normalize=0,\
  aformat=sample_fmts=s16:channel_layouts=mono,alimiter=level_in=1:level_out=0.92,\
  afade=t=out:st=0.66:d=0.06,volume=2.2" -ar 44100 -ac 1 siphon_take.wav
  oggenc -q 3 --serial 1 -o siphon_take.ogg siphon_take.wav
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

- `ward_close.ogg` — the Sphere SEALING (the file keeps the spell's old name), 2.0 s one-shot, heard flat by the caster
  and placed at the head for everyone else. The climb itself has no recording: the glass rises through the cast channel,
  under the channel's own loop, and this is the crown cell landing — a C-major bell chord (C6 G6 C7 E7) with two
  inharmonic partials for the glass, a short high-passed noise "tss", and a soft 55 Hz thump underneath for weight, all
  decaying into silence.

  ```sh
  ffmpeg -f lavfi -i "aevalsrc=exprs='\
   0.20*exp(-t*2.2)*sin(2*PI*1046.5*t)\
  +0.16*exp(-t*2.6)*sin(2*PI*1568*t)\
  +0.14*exp(-t*3.0)*sin(2*PI*2093*t)\
  +0.11*exp(-t*3.6)*sin(2*PI*2637*t)\
  +0.07*exp(-t*4.4)*sin(2*PI*2888*t)\
  +0.05*exp(-t*6.0)*sin(2*PI*5651*t)\
  +0.45*exp(-t*9.0)*sin(2*PI*55*t)':s=44100:d=2.0" \
   -f lavfi -i "anoisesrc=d=2.0:c=white:a=0.8:r=44100:seed=2718" \
   -filter_complex "[1:a]bandpass=f=5200:width_type=o:w=1.6,afade=t=in:st=0:d=0.01,\
  afade=t=out:st=0.02:d=0.42:curve=exp,volume=0.35[ts];\
  [0:a][ts]amix=inputs=2:normalize=0,aformat=sample_fmts=s16:channel_layouts=mono,\
  alimiter=level_in=1:level_out=0.72,afade=t=out:st=1.8:d=0.2" -fflags +bitexact -ar 44100 -ac 1 ward_close.wav
  oggenc -q 3 -s 2718 -o ward_close.ogg ward_close.wav
  ```

## Wanted (placeholders in use)

The Arcana cast set currently reuses existing recordings (see `digblock-client/src/audio.rs::bytes`): `CastLoop` plays
`siphon_draw.ogg`, `TeleportOut` plays `siphon_take.ogg`, `TeleportIn` plays `level_unlock.ogg`. Wanted, CC0: a soft
arcane wind loop for the channel, a release "whoosh" for departure, and a settling chime for arrival — swap the `bytes`
arms when they land, nothing else moves.
