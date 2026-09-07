# Material textures

Run `cargo run -p texgen` from the workspace root and commit the generated PNGs with their recipes. All textures remain
64×64, with RGB for opaque surfaces and emission masks, and RGBA for translucent materials and sprites.

`surfaces.rs` generates terrain, worked stone, metal, foliage, deposits, lamps and tool glows. Broad forms identify the
material at a distance; quantized tones, bevels and chips supply close detail. Each recipe owns stable seeds. A new
recipe must not consume another recipe's random stream. Live and depleted deposits share a mask, so draining a node
changes its colour and light without moving its mineral inclusions. Live nodes retain their host’s albedo; separate
`*Emission.png` masks make only the mineral inclusions fullbright. The GPU atlas has a matching second bank for masks,
leaving surface alpha available for transparency and icons. World and held-item shaders share this separation.

`main.rs` assembles the catalog and draws the essence chips and spell silhouettes. Dirt, grass, logs and lumber now
share the same procedural palette and pixel-cluster treatment as the other surfaces. Grass sides expose the same dirt as
the standalone soil; green and dry turf share their fringe geometry. Log ends carry growth rings inside a bark rim, and
the three lumber colours share restrained planed grain. Heartwood and torches use the generated timber directly, so a
clean generation run never depends on stale PNGs. Water remains untouched source art. Clean glass, tinted panes and
wardglass retain their flat transmission: repeated painted frames obscure the world behind large panes and spell shells.

Metal and polished stone get their shine from `finish_common.wgsl`, not from a painted reflection. The mesher's visual
finish bits follow the texture on cubes, slabs, stairs and doors, and both hand shaders apply the same finish to held
items and to other players' hands. Metal has a tighter, stronger highlight; polished stone has a broader, weaker one.
The sun lobe follows the traced sun visibility. Local light is the probe cache read along the mirrored view ray plus
unshadowed Blinn highlights from the head of the light list, which is flushed nearest the eye first for exactly that
read: no per-fragment light scan and no shadow march, so a lamp behind a thin wall can still gleam on a metal floor.
Matte surfaces skip this work. This is a surface highlight model, not a mirror or screen-space reflection pass.

## Visual review

Inspect each material at native size, reduced size and in a repeated tile. Compare node pairs side by side, and view
sprites over both light and dark backgrounds. Texture means feed the GI material table, so changes to lamp colour or
coverage also change the light they cast. Author emissive art with headroom for `emissive_ink`.

The `materials` scene places matching tiled walls and floors under the same lamps. Facing forward, the panels are metal
on the left, polished stone in the centre and stone on the right. It is available on desktop as `DIG_MATERIALS=1` and on
web as `?materials`. Example web captures:

```sh
just web-shot --materials --seed 7 --tod 30 --frames 180 --pitch -10 --yaw 0 --screenshot target/materials-day.png
just web-shot --materials --seed 7 --tod 180 --frames 180 --pitch -10 --yaw 0 --screenshot target/materials-night.png
```

Use the existing `orbit` or `dolly` capture knobs to check that highlights move with the camera. Check transparent and
emissive materials in the game as well as in a PNG viewer: the latter cannot show transmission, ink gain or GI.
