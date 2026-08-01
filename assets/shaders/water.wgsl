// Water shader — fork of chunk.wgsl with a two-word vertex: word 0 is
// the solid packing (position/normal/corner/texture; ao and tint bits
// unused), word 1 carries per-vertex surface data. Bits 0-3 of word 1
// are the drop code d: the vertex sinks d/9 below its lattice height, so
// a source surface sits at 8/9 and thinner flowing water lower, with
// corner heights shared between adjacent cells (no cracks). Bit 4 marks
// falling water; bits 5-7 flow dir and bit 8 foam are reserved for the
// animation pass. Lighting matches chunk.wgsl minus AO/tint; alpha
// comes from misc.x (the water frame UBO copy).

// Must match FrameUniforms in renderer.rs and the Frame struct in
// sky.wgsl — field order and packing are load-bearing.
struct Frame {
	view_proj: mat4x4f,
	inv_view_proj: mat4x4f,
	light_near: mat4x4f,
	light_wide: mat4x4f,
	// xyz = active caster dir (sun or moon), toward the ground
	// w = shadow strength 0..1 (twilight fade)
	light_dir: vec4f,
	// rgb = direct light color (sun by day, moonlight by night)
	light_color: vec4f,
	// rgb = sky-tinted ambient floor
	ambient: vec4f,
	// rgb = fog color (sky horizon); w = fog start distance (units)
	fog_color: vec4f,
	// xyz = camera position; w = 1 / (fog_end − fog_start)
	cam_pos: vec4f,
	// xyz = direction toward the sun in the sky; w = star intensity
	sun_dir: vec4f,
	// xyz = direction toward the moon; w = sunset glow strength
	moon_dir: vec4f,
	// rgb = sky zenith color
	zenith: vec4f,
	// rgb = sky horizon color
	horizon: vec4f,
	// x = alpha (0.7 for water)
	// y = shadows enabled (0.0 until the shadow passes exist)
	// z = shadow map texel size (1.0 / resolution, both cascades)
	// w = wide-cascade texel size in world units (normal-offset bias)
	misc: vec4f,
}

@group(0) @binding(0) var<uniform> frame: Frame;

@group(1) @binding(0) var blocks_tex: texture_2d_array<f32>;
@group(1) @binding(1) var blocks_smp: sampler;
@group(1) @binding(2) var shadow_near: texture_depth_2d;
@group(1) @binding(3) var shadow_wide: texture_depth_2d;
@group(1) @binding(4) var shadow_smp: sampler_comparison;

struct ChunkU {
	origin: vec4f,
}

@group(2) @binding(0) var<uniform> chunk: ChunkU;

// Java normal() table order (VertTex.glsl):
// 0=W(+x) 1=N(+z) 2=S(-z) 3=B(-y) 4=T(+y) 5=E(-x)
const NORMALS = array<vec3f, 6>(
	vec3f(1.0, 0.0, 0.0),
	vec3f(0.0, 0.0, 1.0),
	vec3f(0.0, 0.0, -1.0),
	vec3f(0.0, -1.0, 0.0),
	vec3f(0.0, 1.0, 0.0),
	vec3f(-1.0, 0.0, 0.0),
);

// Face-axis ambient, matching chunk.wgsl.
const AXIS = array<f32, 6>(0.85, 0.78, 0.78, 0.58, 1.0, 0.85);

struct VsOut {
	@builtin(position) pos: vec4f,
	@location(0) uv: vec2f,
	@location(1) @interpolate(flat) layer: u32,
	@location(2) @interpolate(flat) direct: f32,
	@location(3) sc_near: vec3f,
	@location(4) sc_wide: vec3f,
	@location(5) fog_dist: f32,
	@location(6) @interpolate(flat) axis: f32,
}

@vertex
fn vs(@location(0) packed: vec2u) -> VsOut {
	let w0 = packed.x;
	let x = f32(w0 & 0x1fu);
	var y = f32((w0 >> 5u) & 0x1fu);
	let z = f32((w0 >> 10u) & 0x1fu);
	// Surface drop: 0 = full block height (falling/submerged water and
	// all bottom-plane vertices), else the shared corner surface.
	y -= f32(packed.y & 0xfu) / 9.0;
	let ni = (w0 >> 15u) & 7u;
	let n = NORMALS[ni];
	let world = vec3f(x, y, z) + chunk.origin.xyz;

	var o: VsOut;
	o.pos = frame.view_proj * vec4f(world, 1.0);
	o.uv = vec2f(f32((w0 >> 20u) & 1u), f32((w0 >> 21u) & 1u));
	o.layer = (w0 >> 23u) & 63u;

	o.direct = max(dot(n, -frame.light_dir.xyz), 0.0);
	o.fog_dist = distance(world, frame.cam_pos.xyz);
	o.axis = AXIS[ni];

	let pn = frame.light_near * vec4f(world, 1.0);
	o.sc_near = vec3f(pn.xy * vec2f(0.5, -0.5) + 0.5, pn.z);
	// Wide-cascade normal offset — see chunk.wgsl.
	let pw = frame.light_wide * vec4f(world + n * frame.misc.w, 1.0);
	o.sc_wide = vec3f(pw.xy * vec2f(0.5, -0.5) + 0.5, pw.z);
	return o;
}

// 5-tap cross PCF, identical to chunk.wgsl.
fn pcf5(tex: texture_depth_2d, sc: vec3f) -> f32 {
	let px = frame.misc.z;
	var v = textureSampleCompare(tex, shadow_smp, sc.xy, sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(px, 0.0), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(-px, 0.0), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(0.0, px), sc.z);
	v += textureSampleCompare(tex, shadow_smp, sc.xy + vec2f(0.0, -px), sc.z);
	return v / 5.0;
}

fn shadow_vis(sc_near: vec3f, sc_wide: vec3f, direct: f32) -> f32 {
	if direct <= 0.0 {
		return 0.0;
	}
	let wsc = vec3f(sc_wide.xy, sc_wide.z - 0.0001);
	let d = max(abs(sc_near.x - 0.5), abs(sc_near.y - 0.5)) * 2.0;
	if d >= 1.0 {
		return pcf5(shadow_wide, wsc);
	}
	let near = pcf5(shadow_near, sc_near);
	let t = smoothstep(0.85, 1.0, d);
	if t <= 0.0 {
		return near;
	}
	return mix(near, pcf5(shadow_wide, wsc), t);
}

@fragment
fn fs(i: VsOut) -> @location(0) vec4f {
	let tex = textureSample(blocks_tex, blocks_smp, i.uv, i.layer).rgb;
	var vis = 1.0;
	if frame.misc.y > 0.5 {
		vis = shadow_vis(i.sc_near, i.sc_wide, i.direct);
	}
	let sh = mix(1.0, vis, frame.light_dir.w);
	var col = tex * (frame.ambient.rgb * i.axis + frame.light_color.rgb * i.direct * sh);
	let f = clamp((i.fog_dist - frame.fog_color.w) * frame.cam_pos.w, 0.0, 1.0);
	col = mix(col, frame.fog_color.rgb, f * f * (3.0 - 2.0 * f));
	return vec4f(col, frame.misc.x);
}
