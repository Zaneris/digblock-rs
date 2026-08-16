//! Web audio filler: the WebAudio mirror of apps/desktop/src/audio.rs.
//! Commands apply synchronously to the node graph (no thread, no
//! channel — WebAudio calls are cheap main-thread JS); decoding runs
//! async up front and commands arriving before it finishes are queued.
//!
//! Graph per positional voice: source → panner (equalpower, inverse
//! distance tuned to match the desktop rolloff) → voice gain → SFX
//! bus → destination. Non-positional voices skip the panner. Music
//! tracks route through their own bus; the two bus gains are the user
//! volume sliders.
//!
//! Chrome refuses audio before a user gesture: one-time pointerdown +
//! keydown listeners call `ctx.resume()` — self-contained here, the
//! platform crate stays audio-free.

// web-sys 0.3.103 blanket-marks the legacy WebAudio methods (`stop`,
// `set_onended`, `AudioListener::set_position`, ...) deprecated. Chrome
// — the only wasm target — supports them fully, and they are far
// simpler than the AudioParam automation equivalents.
#![allow(deprecated)]

use digblock_render::audio::{ALL_SFX, AudioCmd, AudioSink, LoopId, Sfx, bytes};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
	AudioBuffer, AudioBufferSourceNode, AudioContext, DistanceModelType, GainNode, PannerNode,
	PanningModelType,
};

const MUSIC_GAIN: f32 = 0.35;

struct Voice {
	src: AudioBufferSourceNode,
	gain: GainNode,
	panner: Option<PannerNode>,
}

struct State {
	ctx: AudioContext,
	/// User-volume buses (settings sliders): every SFX voice hangs off
	/// `sfx_bus`, every music track off `music_bus`, both → destination.
	/// One `set_value` retunes all running voices — no per-voice sweep.
	sfx_bus: GainNode,
	music_bus: GainNode,
	buffers: HashMap<Sfx, AudioBuffer>,
	ready: bool,
	/// Loop commands that arrived before decoding finished (stale
	/// one-shots are dropped — replaying a click seconds late is worse
	/// than skipping it).
	pending: Vec<AudioCmd>,
	loops: HashMap<LoopId, Voice>,
	/// Music pools, [day, night], each shuffled independently.
	pools: [Pool; 2],
	/// Which pool the world clock currently wants (`AudioCmd::MusicSet`).
	/// Only consulted when a track ends, so dusk never cuts one short.
	night: bool,
	music_playing: bool,
}

/// A shuffled music pool. Tracks decode one at a time, so the order is
/// re-rolled on each arrival rather than once at the end — there is no
/// "playlist complete" moment to hook.
#[derive(Default)]
struct Pool {
	tracks: Vec<AudioBuffer>,
	order: Vec<usize>,
	next: usize,
	seed: u64,
}

impl Pool {
	fn push(&mut self, buf: AudioBuffer) {
		self.tracks.push(buf);
		self.order = digblock_render::audio::shuffle_order(self.seed, self.tracks.len());
		self.next = 0;
	}

	/// The next track, wrapping forever; `None` only when empty.
	fn take_next(&mut self) -> Option<AudioBuffer> {
		let i = *self.order.get(self.next % self.order.len().max(1))?;
		self.next = self.next.wrapping_add(1);
		self.tracks.get(i).cloned()
	}
}

pub struct WebSink(Rc<RefCell<State>>);

impl AudioSink for WebSink {
	fn send(&mut self, cmd: AudioCmd) {
		apply(&self.0, cmd);
	}
}

/// Build the graph, kick off async decode + music fetch, hook the
/// autoplay-resume gestures. Any failure degrades to silence.
pub fn start(master_url: Option<String>) -> Box<dyn AudioSink> {
	let build = || -> Option<Rc<RefCell<State>>> {
		let ctx = AudioContext::new().ok()?;
		let sfx_bus = ctx.create_gain().ok()?;
		let music_bus = ctx.create_gain().ok()?;
		sfx_bus.connect_with_audio_node(&ctx.destination()).ok()?;
		music_bus.connect_with_audio_node(&ctx.destination()).ok()?;
		Some(Rc::new(RefCell::new(State {
			ctx,
			sfx_bus,
			music_bus,
			buffers: HashMap::new(),
			ready: false,
			pending: Vec::new(),
			loops: HashMap::new(),
			// Separate seeds so day and night don't march in lockstep.
			// js_sys::Math::random is the only entropy source here that
			// doesn't pull in a wasm RNG dependency.
			pools: [
				Pool {
					seed: (js_sys::Math::random() * u64::MAX as f64) as u64,
					..Pool::default()
				},
				Pool {
					seed: (js_sys::Math::random() * u64::MAX as f64) as u64,
					..Pool::default()
				},
			],
			night: false,
			music_playing: false,
		})))
	};
	let state = build();
	let Some(state) = state else {
		log::warn!("WebAudio unavailable — running silent");
		struct Null;
		impl AudioSink for Null {
			fn send(&mut self, _cmd: AudioCmd) {}
		}
		return Box::new(Null);
	};

	hook_resume_gestures(&state);
	wasm_bindgen_futures::spawn_local(decode_all(state.clone()));
	if let Some(url) = master_url {
		wasm_bindgen_futures::spawn_local(fetch_music(state.clone(), url));
	}
	Box::new(WebSink(state))
}

/// One-time (self-removing via flag) gesture hooks that resume the
/// context — resuming a running context is a no-op, so firing more than
/// once is harmless. The closure lives for the app's lifetime.
fn hook_resume_gestures(state: &Rc<RefCell<State>>) {
	let Some(window) = web_sys::window() else {
		return;
	};
	let st = state.clone();
	let cb = Closure::<dyn FnMut()>::new(move || {
		let s = st.borrow();
		let _ = s.ctx.resume();
	});
	for ev in ["pointerdown", "keydown"] {
		let _ = window.add_event_listener_with_callback(ev, cb.as_ref().unchecked_ref());
	}
	cb.forget();
}

async fn decode(ctx: &AudioContext, raw: &[u8]) -> Option<AudioBuffer> {
	// Uint8Array::from copies into JS; decodeAudioData detaches ITS copy.
	let array = js_sys::Uint8Array::from(raw);
	let promise = ctx.decode_audio_data(&array.buffer()).ok()?;
	JsFuture::from(promise).await.ok()?.dyn_into().ok()
}

async fn decode_all(state: Rc<RefCell<State>>) {
	let ctx = state.borrow().ctx.clone();
	for s in ALL_SFX {
		match decode(&ctx, bytes(s)).await {
			Some(buf) => {
				state.borrow_mut().buffers.insert(s, buf);
			}
			None => log::warn!("sfx {s:?} failed to decode"),
		}
	}
	let pending = {
		let mut st = state.borrow_mut();
		st.ready = true;
		std::mem::take(&mut st.pending)
	};
	for cmd in pending {
		apply(&state, cmd);
	}
}

/// Fetch the master's music manifest + tracks; decoded tracks feed the
/// looping playlist. Any failure = no music, nothing else affected.
async fn fetch_music(state: Rc<RefCell<State>>, master_url: String) {
	let base = master_url.trim_end_matches('/').to_string();
	let manifest = match fetch_json(&format!("{base}/api/music")).await {
		Some(v) => v,
		None => {
			log::warn!("music manifest fetch failed");
			return;
		}
	};
	let Some(tracks) = manifest.as_array().cloned() else {
		return;
	};
	let ctx = state.borrow().ctx.clone();
	for t in tracks {
		let Some(url) = t.get("url").and_then(|u| u.as_str()) else {
			continue;
		};
		// A master predating the day/night split sends no "set"; treat
		// everything it offers as day music rather than dropping it.
		let night = t.get("set").and_then(|s| s.as_str()) == Some("night");
		let url = if url.starts_with("http") {
			url.to_string()
		} else {
			format!("{base}/{}", url.trim_start_matches('/'))
		};
		if let Some(raw) = fetch_bytes(&url).await
			&& let Some(buf) = decode(&ctx, &raw).await
		{
			state.borrow_mut().pools[usize::from(night)].push(buf);
			// Start on the first track that lands, not once the whole
			// playlist has downloaded — several MB of music would
			// otherwise mean minutes of silence. A no-op once playing.
			play_next_track(&state);
		}
	}
}

async fn fetch_response(url: &str) -> Option<web_sys::Response> {
	let window = web_sys::window()?;
	let resp = JsFuture::from(window.fetch_with_str(url)).await.ok()?;
	let resp: web_sys::Response = resp.dyn_into().ok()?;
	resp.ok().then_some(resp)
}

async fn fetch_json(url: &str) -> Option<serde_json::Value> {
	let resp = fetch_response(url).await?;
	let text = JsFuture::from(resp.text().ok()?).await.ok()?;
	serde_json::from_str(&text.as_string()?).ok()
}

async fn fetch_bytes(url: &str) -> Option<Vec<u8>> {
	let resp = fetch_response(url).await?;
	let buf = JsFuture::from(resp.array_buffer().ok()?).await.ok()?;
	Some(js_sys::Uint8Array::new(&buf).to_vec())
}

/// Start the next playlist track; each track's `ended` event chains the
/// following one (closure per track — bounded by playlist length per
/// play, negligible).
fn play_next_track(state: &Rc<RefCell<State>>) {
	let mut st = state.borrow_mut();
	if st.music_playing {
		return;
	}
	// Pool choice happens here, at the track boundary — never mid-track.
	// Falls back to the other pool when the active one is empty, so a
	// master with no night tracks still plays music at night.
	let active = usize::from(st.night);
	let Some(buf) = st.pools[active]
		.take_next()
		.or_else(|| st.pools[1 - active].take_next())
	else {
		return;
	};
	let (Ok(src), Ok(gain)) = (st.ctx.create_buffer_source(), st.ctx.create_gain()) else {
		return;
	};
	src.set_buffer(Some(&buf));
	gain.gain().set_value(MUSIC_GAIN);
	if src.connect_with_audio_node(&gain).is_err()
		|| gain.connect_with_audio_node(&st.music_bus).is_err()
		|| src.start().is_err()
	{
		return;
	}
	st.music_playing = true;
	drop(st);
	let st2 = state.clone();
	let cb = Closure::<dyn FnMut()>::new(move || {
		st2.borrow_mut().music_playing = false;
		play_next_track(&st2);
	});
	src.set_onended(Some(cb.as_ref().unchecked_ref()));
	cb.forget();
}

fn make_voice(
	st: &State,
	sound: Sfx,
	pos: Option<[f32; 3]>,
	gain: f32,
	looped: bool,
) -> Option<Voice> {
	let buf = st.buffers.get(&sound)?;
	let src = st.ctx.create_buffer_source().ok()?;
	src.set_buffer(Some(buf));
	src.set_loop(looped);
	let g = st.ctx.create_gain().ok()?;
	g.gain().set_value(gain);
	let panner = match pos {
		Some(p) => {
			let pan = st.ctx.create_panner().ok()?;
			pan.set_panning_model(PanningModelType::Equalpower);
			pan.set_distance_model(DistanceModelType::Inverse);
			pan.set_ref_distance(2.0);
			pan.set_max_distance(64.0);
			pan.set_rolloff_factor(1.2);
			set_pos(&pan, p);
			src.connect_with_audio_node(&pan).ok()?;
			pan.connect_with_audio_node(&g).ok()?;
			Some(pan)
		}
		None => {
			src.connect_with_audio_node(&g).ok()?;
			None
		}
	};
	g.connect_with_audio_node(&st.sfx_bus).ok()?;
	src.start().ok()?;
	Some(Voice {
		src,
		gain: g,
		panner,
	})
}

fn set_pos(pan: &PannerNode, p: [f32; 3]) {
	pan.position_x().set_value(p[0]);
	pan.position_y().set_value(p[1]);
	pan.position_z().set_value(p[2]);
}

fn stop_voice(v: &Voice) {
	let _ = v.src.stop();
}

fn apply(state: &Rc<RefCell<State>>, cmd: AudioCmd) {
	let mut st = state.borrow_mut();
	// Volume needs no decoded buffers — apply even before `ready`, so a
	// saved setting takes effect from the very first sound.
	if let AudioCmd::Volume { music, sfx } = cmd {
		st.music_bus.gain().set_value(music);
		st.sfx_bus.gain().set_value(sfx);
		return;
	}
	// Likewise a flag, not a sound: the engine sends the first one during
	// startup, long before any buffer has decoded.
	if let AudioCmd::MusicSet { night } = cmd {
		st.night = night;
		return;
	}
	if !st.ready {
		// Queue everything that matters long-term; drop stale one-shots.
		if !matches!(cmd, AudioCmd::Sfx { .. }) {
			st.pending.push(cmd);
		}
		return;
	}
	match cmd {
		AudioCmd::Listener { pos, forward } => {
			let l = st.ctx.listener();
			l.set_position(pos[0] as f64, pos[1] as f64, pos[2] as f64);
			l.set_orientation(
				forward[0] as f64,
				forward[1] as f64,
				forward[2] as f64,
				0.0,
				1.0,
				0.0,
			);
		}
		AudioCmd::Sfx { sound, pos, gain } => {
			// One-shots stop themselves; node GC reclaims the graph.
			let _ = make_voice(&st, sound, pos, gain, false);
		}
		AudioCmd::LoopStart {
			id,
			sound,
			pos,
			gain,
		} => {
			if let Some(old) = st.loops.remove(&id) {
				stop_voice(&old);
			}
			if let Some(v) = make_voice(&st, sound, pos, gain, true) {
				st.loops.insert(id, v);
			}
		}
		AudioCmd::LoopMove { id, pos, gain } => {
			if let Some(v) = st.loops.get(&id) {
				if let Some(pan) = &v.panner {
					set_pos(pan, pos);
				}
				v.gain.gain().set_value(gain);
			}
		}
		AudioCmd::LoopPitch { id, rate } => {
			if let Some(v) = st.loops.get(&id) {
				v.src.playback_rate().set_value(rate);
			}
		}
		AudioCmd::LoopStop { id } => {
			if let Some(v) = st.loops.remove(&id) {
				stop_voice(&v);
			}
		}
		AudioCmd::Pause => {
			let _ = st.ctx.suspend();
		}
		AudioCmd::Resume => {
			let _ = st.ctx.resume();
		}
		// Handled before the ready gate above.
		AudioCmd::Volume { .. } | AudioCmd::MusicSet { .. } => {}
	}
}
