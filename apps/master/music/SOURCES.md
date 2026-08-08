# Music provenance

Served by the master at `/music/*` (manifest at `/api/music`); clients
fetch at runtime — these files are never embedded in game binaries.
Re-encoded to Ogg Vorbis (oggenc q5). All CC0 — no attribution required
(given anyway).

A `night_` filename prefix puts a track in the night pool (the client's
world clock switches pools at 18:00 and 06:00); everything else is day
music. The prefix, rather than a subdirectory, because `/music/{file}`
rejects anything path-shaped — see the note in `Program.cs`. Adding a
track is dropping an `.ogg` here plus a row below; the csproj globs
`music/**` and no code changes.

## Day

- `field_of_dreams.ogg` — "The Field Of Dreams" by pauliuw, CC0.
  https://opengameart.org/content/the-field-of-dreams
- `loading_loop.ogg` — "Loading Screen Loop", CC0.
  https://opengameart.org/content/loading-screen-loop
- `snowfall.ogg` — "Snowfall" (looped version) by Kistol, CC0.
  https://opengameart.org/content/snowfall
- `town_theme.ogg` — "Town Theme RPG" by cynicmusic (pixelsphere.org),
  CC0. https://opengameart.org/content/town-theme-rpg

## Night

- `night_cave_theme.ogg` — "Cave Theme" by Brandon75689, dual
  OGA-BY 3.0 / CC0 (taken under CC0).
  https://opengameart.org/content/cave-theme
- `night_creepy_forest.ogg` — "Creepy Forest (F)" by Augmentality
  (Brandon Morris), dual OGA-BY 3.0 / CC0 (taken under CC0).
  https://opengameart.org/content/creepy-forest-f
- `night_crystal_cave.ogg` — "Crystal Cave (song18)" by cynicmusic
  (pixelsphere.org), CC0. https://opengameart.org/content/crystal-cave-song18
- `night_mysterious_ambience.ogg` — "Mysterious Ambience (song21)" by
  cynicmusic (pixelsphere.org), CC0.
  https://opengameart.org/content/mysterious-ambience-song21
- `night_tragic_ambient.ogg` — "Tragic ambient main menu" by
  brandon75689, dual OGA-BY 3.0 / CC0 (taken under CC0).
  https://opengameart.org/content/tragic-ambient-main-menu
