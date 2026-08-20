// The idle outline — the bare-aim half of the highlight (shared stage in
// highlight_common.wgsl). Deliberately a SEPARATE module from the crack:
// this one must stay free of the fracture's dynamically-indexed
// function-scope arrays, the construct a mobile driver is likeliest to
// refuse — and an invalid pipeline poisons the frame's whole encoder, so
// a driver that chokes on the crack must not be able to black out a
// player who merely aims. Photograph the crack half with
// DIG_CRACK=<percent>.

@fragment
fn fs(in: VsOut) -> @location(0) vec4f {
	return vec4f(0.0, 0.0, 0.0, hl_base(in).alpha);
}
