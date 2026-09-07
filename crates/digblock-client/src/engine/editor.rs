//! The shape editor (DIG_EDITOR / ?editor): sculpt sub-block objects the way
//! blocks are placed and dug, at any of the twelve-grid's exact resolutions
//! (full/halves/thirds/quarters/sixths/twelfths), and save them as `.shape`
//! files the game loads. The work in progress renders through the game's own
//! mesher: the footprint's cells are real Chair-row cells on a pad whose
//! baked variants are hijacked via `shapes::set_preview_override`, so what
//! you sculpt is exactly what ships — textures, culling and lighting
//! included. Sculpt targeting runs `digblock_core::raycast` in the volume's
//! own sub-voxel space (twelve to the block), against the document grid and
//! a virtual ground plane under it. Keyboard-first; the side panel lists the
//! bindings. Offline-only by construction: the knob starts a local world and
//! preview overrides never exist in a networked session.

use super::*;
use digblock_core::shapes::def::{Roles, StateModel};
use digblock_core::shapes::{self, GRID};

mod doc;
pub(crate) mod io;

use doc::{ShapeDoc, Voxel};

/// How far the sculpt ray reaches, in blocks (its own constant — the
/// game's `REACH` is a gameplay rule, not an editor convenience).
const EDIT_REACH: f32 = 24.0;
/// Brush edge per resolution key 1..=6, in twelfths.
const STEPS: [u8; 6] = [12, 6, 4, 3, 2, 1];
/// Undo depth (full-volume snapshots; a cell is ~30KB).
const UNDO_CAP: usize = 64;

/// Brush role presets cycled by F, with the labels the panel shows.
const ROLE_PRESETS: [(&str, Roles); 4] = [
	("all", Roles::ALL),
	(
		"visual",
		Roles {
			visual: true,
			collision: false,
			light: false,
			interaction: false,
		},
	),
	(
		"visual+interaction",
		Roles {
			visual: true,
			collision: false,
			light: false,
			interaction: true,
		},
	),
	(
		"collision+light+interaction",
		Roles {
			visual: false,
			collision: true,
			light: true,
			interaction: true,
		},
	),
];

pub(super) struct EditorState {
	pub doc: ShapeDoc,
	/// World cell of the footprint's canonical part 0.
	origin: (i32, i32, i32),
	/// Brush edge in twelfths (a STEPS entry).
	step: u8,
	role_preset: usize,
	/// 0 = the block row's default texture, else `tex::NAMED[i - 1]`.
	tex_sel: usize,
	/// Preview facing k (the document stays canonical +x).
	facing_k: u8,
	/// Show every compiled box as an outline (B), instead of the brush cursor.
	boxes_view: bool,
	undo: Vec<Vec<Vec<doc::VoxelGrid>>>,
	redo: Vec<Vec<Vec<doc::VoxelGrid>>>,
	/// Built-ins then disk names; `file_sel` indexes it.
	files: Vec<(String, bool)>,
	file_sel: usize,
	/// Live rename buffer (N); None = not renaming.
	naming: Option<String>,
	status: String,
	/// Worst per-cell baked quad count of the last preview.
	quads_worst: usize,
	/// Ramps and folded triangles in the worst variant, for the HUD.
	ramps_worst: usize,
	tris_worst: usize,
	ctrl: bool,
	shift: bool,
	prev_dig: bool,
	pub(super) preview_dirty: bool,
	/// Cover-table entries the last preview rebuild owes the GPU, one per
	/// footprint part (`gi::preview_cover_slot`): the sculpt's own light
	/// boxes, so it shades and self-shadows as itself rather than as the
	/// chair whose registry entry it borrows. Flushed where the device is
	/// in hand (`editor_flush_covers`), since the rebuild runs without it.
	pub(super) pending_covers: Vec<(u8, Option<digblock_core::gi::Cover>)>,
}

impl EditorState {
	pub(super) fn new(origin: (i32, i32, i32)) -> Self {
		let files = builtin_files();
		EditorState {
			doc: ShapeDoc::new("newshape"),
			origin,
			step: 4,
			role_preset: 0,
			tex_sel: 0,
			facing_k: 0,
			boxes_view: false,
			undo: Vec::new(),
			redo: Vec::new(),
			files,
			file_sel: 0,
			naming: None,
			status: String::from("shape editor: [ ] pick file, Enter load, Ctrl+S save"),
			quads_worst: 0,
			ramps_worst: 0,
			tris_worst: 0,
			ctrl: false,
			shift: false,
			prev_dig: false,
			preview_dirty: true,
			pending_covers: Vec::new(),
		}
	}

	fn brush(&self) -> Voxel {
		let tex = if self.tex_sel == 0 {
			[None; 3]
		} else {
			[Some(digblock_core::block::tex::NAMED[self.tex_sel - 1].1); 3]
		};
		Voxel {
			roles: ROLE_PRESETS[self.role_preset].1,
			tex,
		}
	}
}

/// Built-in roster rows: (name, is_builtin).
fn builtin_files() -> Vec<(String, bool)> {
	shapes::def::ASSETS
		.iter()
		.map(|&(n, _)| (n.to_string(), true))
		.collect()
}

/// Rotate a footprint-local point k × 90° CCW about +y — the whole-volume
/// twin of `rotate_boxes`: (x, z) → (12 − z, x), which lands part offsets
/// exactly where the game's own facing rotation puts them.
fn vol_to_world(p: Vec3, k: u8) -> Vec3 {
	let mut p = p;
	for _ in 0..(k & 3) {
		p = Vec3::new(f32::from(GRID) - p.z, p.y, p.x);
	}
	p
}

/// Inverse of [`vol_to_world`] (pass the SAME k).
fn world_to_vol(p: Vec3, k: u8) -> Vec3 {
	let mut p = p;
	for _ in 0..(k & 3) {
		p = Vec3::new(p.z, p.y, f32::from(GRID) - p.x);
	}
	p
}

/// [`world_to_vol`] for a DIRECTION: the rotation without the translation.
fn world_to_vol_dir(d: Vec3, k: u8) -> Vec3 {
	let mut d = d;
	for _ in 0..(k & 3) {
		d = Vec3::new(d.z, d.y, -d.x);
	}
	d
}

impl Engine {
	/// The editor's whole per-frame update: fly camera, IO responses,
	/// sculpt input, preview rebuild. Replaces the interactive half of
	/// `step` — dig/place/net/tutorial never run in editor mode.
	pub(super) fn editor_step(&mut self, dt: f32) {
		const LOOK_SENS: f32 = 0.12;
		let (lx, mut ly) = std::mem::take(&mut self.input.look_accum);
		if self.config.invert_mouse_y {
			ly = -ly;
		}
		self.look.look(lx * LOOK_SENS, ly * LOOK_SENS);
		self.camera.yaw_deg = self.look.yaw_deg;
		self.camera.pitch_deg = self.look.pitch_deg;
		let speed = 8.0 * dt;
		let dir = self.camera.dir();
		let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or_zero();
		let right = flat.cross(Vec3::Y);
		let k = &self.input.keys;
		let mut mv = Vec3::ZERO;
		for (on, d) in [
			(k.w, flat),
			(k.s, -flat),
			(k.d, right),
			(k.a, -right),
			(k.up, Vec3::Y),
			(k.down, -Vec3::Y),
		] {
			if on {
				mv += d;
			}
		}
		self.camera.pos += mv.normalize_or_zero() * speed;
		self.editor_pump_io();
		self.editor_sculpt();
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		if std::mem::take(&mut ed.preview_dirty) {
			self.editor_rebuild_preview();
		}
	}

	/// One sculpt ray in the volume's canonical sub-voxel space: the camera
	/// ray is scaled ×12 and inverse-rotated by the preview facing, so the
	/// picture on screen and the grid underneath always agree.
	fn editor_ray(&self) -> Option<RayHit> {
		let ed = self.editor.as_ref()?;
		let o = (self.camera.pos
			- Vec3::new(ed.origin.0 as f32, ed.origin.1 as f32, ed.origin.2 as f32))
			* f32::from(GRID);
		let origin = world_to_vol(o, ed.facing_k);
		let dir = world_to_vol_dir(self.camera.dir(), ed.facing_k);
		let e = ed.doc.extent();
		let stone = digblock_core::block::BlockType::Stone as u16;
		let air = digblock_core::block::BlockType::Air as u16;
		raycast(
			origin,
			dir.normalize_or_zero(),
			EDIT_REACH * f32::from(GRID),
			&|x, y, z| {
				if ed.doc.occupied([x, y, z]) {
					return Some(stone);
				}
				// The virtual ground plane: one sub-voxel of floor under the
				// footprint's columns, so the first material has something
				// to be built against.
				if y == -1 && (0..e[0]).contains(&x) && (0..e[2]).contains(&z) {
					return Some(stone);
				}
				Some(air)
			},
		)
	}

	/// Apply the held/clicked sculpt verbs to the document.
	fn editor_sculpt(&mut self) {
		let place = std::mem::take(&mut self.input.click_place);
		let dig_edge = self.input.dig_held && !self.editor.as_ref().is_some_and(|e| e.prev_dig);
		if let Some(ed) = self.editor.as_mut() {
			ed.prev_dig = self.input.dig_held;
		}
		if !place && !dig_edge {
			return;
		}
		let Some(hit) = self.editor_ray() else {
			return;
		};
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		if ed.naming.is_some() {
			return;
		}
		let snapshot = ed.doc.volumes.clone();
		let changed = if dig_edge {
			let sv = [hit.cell.0, hit.cell.1, hit.cell.2];
			ed.doc.occupied(sv) && ed.doc.apply(sv, ed.step, Voxel::EMPTY)
		} else if hit.normal != (0, 0, 0) {
			let sv = [
				hit.cell.0 + hit.normal.0,
				hit.cell.1 + hit.normal.1,
				hit.cell.2 + hit.normal.2,
			];
			let brush = ed.brush();
			ed.doc.apply(sv, ed.step, brush)
		} else {
			false
		};
		if changed {
			ed.undo.push(snapshot);
			if ed.undo.len() > UNDO_CAP {
				ed.undo.remove(0);
			}
			ed.redo.clear();
			ed.preview_dirty = true;
		}
	}

	/// Recompile the document and re-install the preview: one hijacked
	/// Chair variant per footprint part, cells re-laid for the facing.
	pub(super) fn editor_rebuild_preview(&mut self) {
		use digblock_core::block::BlockType;
		use digblock_core::chunk::make_cell;
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		let list = ed.doc.compile(ed.doc.active);
		let parts = ed.doc.parts();
		let k = ed.facing_k;
		ed.quads_worst = 0;
		// All four chair states get an override: live parts their slice,
		// the rest empty — a stale variant from a larger footprint would
		// otherwise keep drawing.
		ed.pending_covers.clear();
		for p in 0..4u8 {
			let polys = ed
				.doc
				.polys
				.get(ed.doc.active)
				.map_or(&[][..], Vec::as_slice);
			let v = if p < parts {
				shapes::preview_variant(&list, polys, ed.doc.footprint, ed.doc.part_cell(p), k)
			} else {
				shapes::preview_variant(&[], &[], [1, 1, 1], (0, 0, 0), 0)
			};
			if v.quads.len() > ed.quads_worst || p == 0 {
				ed.quads_worst = v.quads.len();
				ed.ramps_worst = v.quads.iter().filter(|q| q.is_ramp()).count();
				ed.tris_worst = v.quads.iter().filter(|q| q.is_tri()).count();
			}
			// The GI sees the sculpt through its part's rewritable cover
			// slot, which `gi_classes` names for an overridden cell.
			ed.pending_covers.push((
				digblock_core::gi::preview_cover_slot(p),
				digblock_core::gi::Cover::from_light_boxes(&v.light),
			));
			shapes::set_preview_override(BlockType::Chair, p, v);
		}
		// Re-lay the world cells (footprint or facing may have moved).
		let origin = ed.origin;
		let mut cells = Vec::new();
		for p in 0..parts {
			let co = ed.doc.part_cell(p);
			let (mut x, mut z) = (co.0, co.2);
			for _ in 0..(k & 3) {
				(x, z) = (-z, x);
			}
			cells.push((
				(origin.0 + x, origin.1 + co.1, origin.2 + z),
				make_cell(BlockType::Chair, p),
			));
		}
		// The editor owns the whole object envelope (the pad plan leaves
		// it alone): every envelope cell is either a footprint part or Air.
		let air = BlockType::Air as u16;
		let mut all_landed = true;
		for x in origin.0 - 3..=origin.0 + 3 {
			for y in origin.1..=origin.1 + 3 {
				for z in origin.2 - 3..=origin.2 + 3 {
					let want = cells
						.iter()
						.find(|&&(c, _)| c == (x, y, z))
						.map_or(air, |&(_, v)| v);
					if self.pipeline.cell_at(x, y, z) != Some(want) {
						self.pipeline.set_cell(x, y, z, want);
						// Not resident yet: retry next frame, the chunks
						// stream in behind the pad plan.
						all_landed &= self.pipeline.cell_at(x, y, z) == Some(want);
					}
				}
			}
		}
		// Same-value writes no-op in set_cell, so force the remesh the
		// override swap needs — and the relight: the cells' cover BYTES
		// are stable per part, only the table entries behind them moved.
		self.pipeline.remesh_near(origin, 1);
		for &(c, _) in &cells {
			self.pipeline.touch_gi(c.0, c.1, c.2);
		}
		if let Some(ed) = self.editor.as_mut() {
			ed.preview_dirty = !all_landed;
		}
	}

	/// This frame's highlight boxes: the snapped brush cursor (place face
	/// in place mode), or every compiled box under the boxes view.
	pub(super) fn editor_fill_highlights(&mut self) {
		let mut out = std::mem::take(&mut self.vfx.highlight_scratch);
		out.clear();
		let hit = self.editor_ray();
		if let Some(ed) = self.editor.as_ref() {
			let g = f32::from(GRID);
			let world_box = |lo: Vec3, hi: Vec3| {
				// Corners through the facing rotation, then min/max — a
				// rotated box's min corner is not the rotated min corner.
				let a = vol_to_world(lo, ed.facing_k);
				let b = vol_to_world(hi, ed.facing_k);
				(a.min(b) / g, a.max(b) / g)
			};
			if ed.boxes_view {
				for b in ed.doc.compile(ed.doc.active) {
					let (lo, hi) = world_box(
						Vec3::new(
							f32::from(b.sub.min[0]),
							f32::from(b.sub.min[1]),
							f32::from(b.sub.min[2]),
						),
						Vec3::new(
							f32::from(b.sub.max[0]),
							f32::from(b.sub.max[1]),
							f32::from(b.sub.max[2]),
						),
					);
					out.push(crate::highlight::HighlightTarget {
						cell: ed.origin,
						lo: [lo.x, lo.y, lo.z],
						hi: [hi.x, hi.y, hi.z],
						progress: None,
					});
				}
			} else if let Some(hit) = hit {
				// The brush cube the next click fills (or clears).
				let cell = if self.input.dig_held || hit.normal == (0, 0, 0) {
					hit.cell
				} else {
					(
						hit.cell.0 + hit.normal.0,
						hit.cell.1 + hit.normal.1,
						hit.cell.2 + hit.normal.2,
					)
				};
				let s = i32::from(ed.step);
				let lo3 = [
					ShapeDoc::snap(cell.0, ed.step),
					ShapeDoc::snap(cell.1, ed.step),
					ShapeDoc::snap(cell.2, ed.step),
				];
				let (lo, hi) = world_box(
					Vec3::new(lo3[0] as f32, lo3[1] as f32, lo3[2] as f32),
					Vec3::new(
						(lo3[0] + s) as f32,
						(lo3[1] + s) as f32,
						(lo3[2] + s) as f32,
					),
				);
				out.push(crate::highlight::HighlightTarget {
					cell: ed.origin,
					lo: [lo.x, lo.y, lo.z],
					hi: [hi.x, hi.y, hi.z],
					progress: None,
				});
			}
		}
		self.vfx.highlight_scratch = out;
	}
}

impl Engine {
	/// Drain shape-service responses into the document / status line.
	fn editor_pump_io(&mut self) {
		let responses: Vec<io::ShapeIoResp> = match self.shape_services.as_ref() {
			Some(sv) => sv.resp_rx.try_iter().collect(),
			None => return,
		};
		let mut relist = false;
		for resp in responses {
			let Some(ed) = self.editor.as_mut() else {
				return;
			};
			match resp {
				io::ShapeIoResp::Listing(Ok(names)) => {
					ed.files = builtin_files();
					for n in names {
						if !ed.files.iter().any(|(f, _)| *f == n) {
							ed.files.push((n, false));
						}
					}
					ed.file_sel = ed.file_sel.min(ed.files.len().saturating_sub(1));
				}
				io::ShapeIoResp::Listing(Err(e)) => ed.status = format!("list: {e}"),
				io::ShapeIoResp::Loaded { name, result } => match result {
					Ok(text) => self.editor_adopt(&name, &text),
					Err(e) => ed.status = format!("load {name}: {e}"),
				},
				io::ShapeIoResp::Saved { name, result } => match result {
					Ok(()) => {
						ed.status = format!("saved {name}");
						ed.doc.dirty = false;
						relist = true;
					}
					Err(e) => ed.status = format!("save {name}: {e}"),
				},
			}
		}
		if relist && let Some(sv) = self.shape_services.as_ref() {
			let _ = sv.cmd_tx.send(io::ShapeIoCmd::List);
		}
	}

	/// Parse loaded text and swap the document in (any error → status).
	fn editor_adopt(&mut self, name: &str, text: &str) {
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		match shapes::def::parse(text) {
			Ok(d) => {
				ed.doc = ShapeDoc::from_def(&d);
				ed.doc.name = name.to_string();
				ed.undo.clear();
				ed.redo.clear();
				ed.facing_k = 0;
				ed.preview_dirty = true;
				ed.status = format!("loaded {name}");
			}
			Err(e) => ed.status = format!("{name}: {e}"),
		}
	}

	/// Load the selected file row: built-ins from the embedded roster,
	/// disk names through the services.
	fn editor_load_selected(&mut self) {
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		let Some((name, builtin)) = ed.files.get(ed.file_sel).cloned() else {
			return;
		};
		if builtin {
			let src = shapes::def::ASSETS
				.iter()
				.find(|&&(n, _)| n == name)
				.map(|&(_, s)| s.to_string());
			if let Some(src) = src {
				self.editor_adopt(&name, &src);
			}
		} else if let Some(sv) = self.shape_services.as_ref() {
			let _ = sv.cmd_tx.send(io::ShapeIoCmd::Load { name });
		}
	}

	/// Emit, validate through the game's own parser, and send the save.
	fn editor_save(&mut self) {
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		let def = ed.doc.to_def();
		let text = shapes::def::emit(&def);
		if let Err(e) = shapes::def::parse(&text) {
			ed.status = format!("not saved, invalid: {e}");
			return;
		}
		let name = ed.doc.name.clone();
		match self.shape_services.as_ref() {
			Some(sv) => {
				let _ = sv.cmd_tx.send(io::ShapeIoCmd::Save { name, text });
			}
			None => ed.status = String::from("no shape store wired (save disabled)"),
		}
	}

	/// Editor input: movement/look/mouse plus the editor's own keys. Runs
	/// INSTEAD of the game's input dispatch — no hotbar, menus or pages.
	pub(super) fn editor_input(&mut self, event: InputEvent) {
		match event {
			InputEvent::MouseDelta { dx, dy } => {
				self.input.look_accum.0 += dx as f32;
				self.input.look_accum.1 += dy as f32;
			}
			InputEvent::MouseButton { button, pressed } => match button {
				MouseButton::Left => self.input.dig_held = pressed,
				MouseButton::Right => {
					if pressed {
						self.input.click_place = true;
					}
				}
				MouseButton::Other => {}
			},
			InputEvent::Text { ch } => {
				if let Some(ed) = self.editor.as_mut()
					&& let Some(buf) = ed.naming.as_mut()
					&& (ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
					&& buf.len() < 24
				{
					buf.push(ch.to_ascii_lowercase());
				}
			}
			InputEvent::Key { key, pressed } => self.editor_key(key, pressed),
			InputEvent::FocusLost => {
				self.input.keys = Default::default();
				if let Some(ed) = self.editor.as_mut() {
					(ed.ctrl, ed.shift, ed.prev_dig) = (false, false, false);
				}
				self.input.dig_held = false;
			}
			_ => {}
		}
	}

	fn editor_key(&mut self, key: Key, pressed: bool) {
		// Held movement + modifiers first; the rest are press edges.
		match key {
			Key::W => self.input.keys.w = pressed,
			Key::A => self.input.keys.a = pressed,
			Key::S => self.input.keys.s = pressed,
			Key::D => self.input.keys.d = pressed,
			Key::Space => self.input.keys.up = pressed,
			Key::Shift => {
				self.input.keys.down = pressed;
				if let Some(ed) = self.editor.as_mut() {
					ed.shift = pressed;
				}
			}
			Key::Ctrl => {
				if let Some(ed) = self.editor.as_mut() {
					ed.ctrl = pressed;
				}
			}
			_ => {}
		}
		if !pressed {
			return;
		}
		let Some(ed) = self.editor.as_mut() else {
			return;
		};
		// Rename capture owns every key while open.
		if ed.naming.is_some() {
			match key {
				Key::Enter => {
					let name = ed.naming.take().unwrap();
					if !name.is_empty() {
						ed.doc.name = name;
						ed.doc.dirty = true;
					}
				}
				Key::Escape => ed.naming = None,
				Key::Backspace => {
					ed.naming.as_mut().map(String::pop);
				}
				_ => {}
			}
			return;
		}
		match key {
			Key::Digit(n @ 1..=6) => {
				ed.step = STEPS[usize::from(n) - 1];
			}
			// Footprint: 7/8/9 bump W/H/D, wrapping under the 4-part cap.
			Key::Digit(n @ 7..=9) => {
				let axis = usize::from(n) - 7;
				let mut f = ed.doc.footprint;
				f[axis] += 1;
				if !ed.doc.resize(f, false) {
					f[axis] = 1;
					if !ed.doc.resize(f, false) {
						ed.status = String::from("shrink refused: cells hold material");
						return;
					}
				}
				ed.preview_dirty = true;
				ed.status = format!("footprint {:?}", ed.doc.footprint);
			}
			Key::R => {
				ed.facing_k = (ed.facing_k + 1) & 3;
				ed.preview_dirty = true;
			}
			Key::F => {
				ed.role_preset = (ed.role_preset + 1) % ROLE_PRESETS.len();
			}
			Key::T => {
				let n = digblock_core::block::tex::NAMED.len() + 1;
				ed.tex_sel = if ed.shift {
					(ed.tex_sel + n - 1) % n
				} else {
					(ed.tex_sel + 1) % n
				};
			}
			Key::B => {
				ed.boxes_view = !ed.boxes_view;
			}
			Key::G => {
				ed.doc.ramp = !ed.doc.ramp;
				ed.doc.dirty = true;
			}
			Key::M => {
				// Cycle the state model; entering explicit replicates the
				// canonical volume, leaving keeps the active one.
				let (next, volumes) = match ed.doc.states {
					StateModel::None_ => (StateModel::Facing, None),
					StateModel::Facing => (StateModel::Door, None),
					StateModel::Door => (
						StateModel::Explicit(4),
						Some(vec![ed.doc.volumes[ed.doc.active].clone(); 4]),
					),
					StateModel::Explicit(_) | StateModel::Forms(_) => (
						StateModel::None_,
						Some(vec![ed.doc.volumes[ed.doc.active].clone()]),
					),
				};
				ed.doc.states = next;
				if let Some(v) = volumes {
					ed.doc.volumes = v;
					ed.doc.active = 0;
				}
				ed.doc.dirty = true;
				ed.preview_dirty = true;
			}
			Key::Comma | Key::Period => {
				let n = ed.doc.volumes.len();
				if n > 1 {
					ed.doc.active = if key == Key::Period {
						(ed.doc.active + 1) % n
					} else {
						(ed.doc.active + n - 1) % n
					};
					ed.preview_dirty = true;
				}
			}
			Key::BracketLeft | Key::BracketRight => {
				let n = ed.files.len();
				if n > 0 {
					ed.file_sel = if key == Key::BracketRight {
						(ed.file_sel + 1) % n
					} else {
						(ed.file_sel + n - 1) % n
					};
				}
			}
			Key::Enter => self.editor_load_selected(),
			Key::N => ed.naming = Some(String::new()),
			Key::O => {
				if let Some(sv) = self.shape_services.as_ref() {
					let _ = sv.cmd_tx.send(io::ShapeIoCmd::PickAndLoad);
					ed.status = String::from("waiting for a file…");
				}
			}
			Key::Z if ed.ctrl => {
				if let Some(prev) = ed.undo.pop() {
					ed.redo.push(std::mem::replace(&mut ed.doc.volumes, prev));
					ed.doc.dirty = true;
					ed.preview_dirty = true;
				}
			}
			Key::Y if ed.ctrl => {
				if let Some(next) = ed.redo.pop() {
					ed.undo.push(std::mem::replace(&mut ed.doc.volumes, next));
					ed.doc.dirty = true;
					ed.preview_dirty = true;
				}
			}
			Key::S if ed.ctrl => self.editor_save(),
			_ => {}
		}
	}

	/// The always-on side panel: state on top, bindings under it.
	pub(super) fn editor_panel(&mut self, size: (u32, u32)) {
		use crate::ui::draw::Layer;
		let Some(ed) = self.editor.as_ref() else {
			return;
		};
		let name = match &ed.naming {
			Some(buf) => format!("name: {buf}_"),
			None => format!("{}{}", ed.doc.name, if ed.doc.dirty { " *" } else { "" }),
		};
		let tex = if ed.tex_sel == 0 {
			"block default"
		} else {
			digblock_core::block::tex::NAMED[ed.tex_sel - 1].0
		};
		let states = match ed.doc.states {
			StateModel::None_ => "none".to_string(),
			StateModel::Facing => "facing".to_string(),
			StateModel::Door => "door".to_string(),
			StateModel::Explicit(n) => format!("explicit {} [{}]", n, ed.doc.active),
			StateModel::Forms(n) => format!("forms {} [{}]", n, ed.doc.active),
		};
		let over = ed.quads_worst > digblock_core::shapes::MAX_SHAPE_QUADS;
		let (fname, builtin) = ed
			.files
			.get(ed.file_sel)
			.map(|(n, b)| (n.as_str(), *b))
			.unwrap_or(("-", false));
		let lines: Vec<(String, bool)> = vec![
			(name, true),
			(
				format!(
					"1/{}  brush {}  tex {tex}",
					12 / ed.step,
					ROLE_PRESETS[ed.role_preset].0
				),
				true,
			),
			(
				format!(
					"footprint {:?}  states {states}{}  facing {}",
					ed.doc.footprint,
					if ed.doc.ramp { "  ramp" } else { "" },
					u32::from(ed.facing_k) * 90,
				),
				true,
			),
			(
				format!(
					"quads {} / {}{}{}{}",
					ed.quads_worst,
					digblock_core::shapes::MAX_SHAPE_QUADS,
					if ed.ramps_worst > 0 {
						format!("  ramps {}", ed.ramps_worst)
					} else {
						String::new()
					},
					if ed.tris_worst > 0 {
						format!("  tris {}", ed.tris_worst)
					} else {
						String::new()
					},
					if over { "  OVER CAP" } else { "" }
				),
				over,
			),
			(
				format!(
					"file {}/{}: {fname}{}",
					ed.file_sel + 1,
					ed.files.len(),
					if builtin { " (built-in)" } else { "" },
				),
				true,
			),
			(ed.status.clone(), true),
			(String::new(), false),
			("LMB remove  RMB place  1-6 resolution".into(), false),
			("F roles  T texture  R rotate  B boxes".into(), false),
			("7/8/9 footprint  M states  , . state  G ramp".into(), false),
			(
				"[ and ] cycle file  Enter load  N name  Ctrl+S save".into(),
				false,
			),
			("O import  Ctrl+Z/Y undo/redo".into(), false),
		];
		let scale = self.ui.ui_scale;
		let (px, line, x) = (14.0 * scale, 18.0 * scale, 10.0 * scale);
		let mut y = 10.0 * scale;
		for (s, bright) in lines {
			let color = if bright {
				0xFF_FF_FF_FFu32
			} else {
				0xFF_B0_B0_B0
			};
			self.ui.ui_dl.text(Layer::Content, x, y, px, color, &s);
			y += line;
		}
		let _ = size;
	}
}
