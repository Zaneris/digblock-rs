//! Desktop audio filler: rodio on a dedicated thread, fed by the
//! engine's `AudioSink` seam over a channel (the mirror of
//! services.rs's thread-per-concern shape). The engine never sees
//! rodio; a dead or deviceless audio thread silently mutes the game
//! instead of failing it.
//!
//! Spatialization: one rodio `SpatialSink` per positional voice. Rodio's
//! two-ear model has a fixed 1/d attenuation with no rolloff knob, so
//! every world coordinate (emitters and ears) is scaled by [`ATT`],
//! stretching the curve to near-silence around 50-60 blocks — matching
//! the visual spray/water-audio radius.
//!
//! Music: fetched from the master server (never embedded) by a helper
//! thread; the audio thread decodes and loops the playlist at low gain.

use digblock_render::audio::{bytes, AudioCmd, AudioSink, LoopId, Sfx};
use rodio::source::{Buffered, Source};
use rodio::{Decoder, OutputStream, Sink, SpatialSink};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::mpsc;
use std::time::Duration;

/// World-to-rodio coordinate scale (see module docs).
const ATT: f32 = 0.08;
/// Ear half-spacing in world blocks.
const EAR_HALF: f32 = 0.2;
const MUSIC_GAIN: f32 = 0.35;

struct ChannelSink(mpsc::Sender<AudioCmd>);

impl AudioSink for ChannelSink {
	fn send(&mut self, cmd: AudioCmd) {
		// A dead audio thread must never kill the game.
		let _ = self.0.send(cmd);
	}
}

/// Spawn the audio thread and return the engine-facing sink.
/// `master_url` enables the music playlist when present.
pub fn start(master_url: Option<String>) -> Box<dyn AudioSink> {
	let (tx, rx) = mpsc::channel::<AudioCmd>();
	let (music_tx, music_rx) = mpsc::channel::<Vec<u8>>();
	if let Some(url) = master_url {
		std::thread::Builder::new()
			.name("music-fetch".into())
			.spawn(move || fetch_music(&url, &music_tx))
			.ok();
	}
	std::thread::Builder::new()
		.name("audio".into())
		.spawn(move || run(rx, music_rx))
		.ok();
	Box::new(ChannelSink(tx))
}

/// Pull the manifest, then each track's bytes (decode happens on the
/// audio thread). Any failure just means no music.
fn fetch_music(master_url: &str, out: &mpsc::Sender<Vec<u8>>) {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Track {
		#[allow(dead_code)]
		name: String,
		url: String,
	}
	let manifest = format!("{}/api/music", master_url.trim_end_matches('/'));
	let tracks: Vec<Track> = match ureq::get(&manifest)
		.call()
		.and_then(|mut r| r.body_mut().read_json())
	{
		Ok(t) => t,
		Err(e) => {
			log::warn!("music manifest {manifest}: {e}");
			return;
		}
	};
	for t in tracks {
		let url = if t.url.starts_with("http") {
			t.url
		} else {
			format!(
				"{}/{}",
				master_url.trim_end_matches('/'),
				t.url.trim_start_matches('/')
			)
		};
		match ureq::get(&url)
			.call()
			.map_err(|e| e.to_string())
			.and_then(|mut r| r.body_mut().read_to_vec().map_err(|e| e.to_string()))
		{
			Ok(b) => {
				if out.send(b).is_err() {
					return;
				}
			}
			Err(e) => log::warn!("music fetch {url}: {e}"),
		}
	}
}

type Cached = Buffered<Decoder<Cursor<&'static [u8]>>>;

enum VoiceSink {
	Spatial(SpatialSink),
	Flat(Sink),
}

/// A live voice plus its authored gain, kept so a later user-volume
/// change can recompute `base_gain * sfx_vol` without a LoopMove.
struct Voice {
	sink: VoiceSink,
	base_gain: f32,
}

impl Voice {
	fn set_speed(&self, v: f32) {
		match &self.sink {
			VoiceSink::Spatial(s) => s.set_speed(v),
			VoiceSink::Flat(s) => s.set_speed(v),
		}
	}
	fn pause(&self) {
		match &self.sink {
			VoiceSink::Spatial(s) => s.pause(),
			VoiceSink::Flat(s) => s.pause(),
		}
	}
	fn play(&self) {
		match &self.sink {
			VoiceSink::Spatial(s) => s.play(),
			VoiceSink::Flat(s) => s.play(),
		}
	}
	fn stop(&self) {
		match &self.sink {
			VoiceSink::Spatial(s) => s.stop(),
			VoiceSink::Flat(s) => s.stop(),
		}
	}
	fn apply_volume(&self, sfx_vol: f32) {
		match &self.sink {
			VoiceSink::Spatial(s) => s.set_volume(self.base_gain * sfx_vol),
			VoiceSink::Flat(s) => s.set_volume(self.base_gain * sfx_vol),
		}
	}
}

fn run(rx: mpsc::Receiver<AudioCmd>, music_rx: mpsc::Receiver<Vec<u8>>) {
	let stream: OutputStream = match rodio::OutputStreamBuilder::open_default_stream() {
		Ok(s) => s,
		Err(e) => {
			log::warn!("audio device unavailable ({e}) — running silent");
			// Drain forever so the engine-side channel never backs up.
			while rx.recv().is_ok() {}
			return;
		}
	};
	let mixer = stream.mixer();

	// Decode every embedded SFX once, up front (~0.5 MB of ogg).
	let mut cache: HashMap<Sfx, Cached> = HashMap::new();
	for s in digblock_render::audio::ALL_SFX {
		match Decoder::new(Cursor::new(bytes(s))) {
			Ok(d) => {
				cache.insert(s, d.buffered());
			}
			Err(e) => log::warn!("sfx {s:?} failed to decode: {e}"),
		}
	}

	let mut loops: HashMap<LoopId, Voice> = HashMap::new();
	let mut oneshots: Vec<Voice> = Vec::new();
	let mut paused = false;
	// User SFX multiplier (settings slider), applied on top of the
	// engine's authored per-voice gains. Music needs no mirror — the
	// music sink itself holds its volume across tracks.
	let mut sfx_vol = 1.0f32;
	// Ears in rodio space, updated by Listener.
	let (mut left_ear, mut right_ear) = ([-EAR_HALF * ATT, 0.0, 0.0], [EAR_HALF * ATT, 0.0, 0.0]);
	// Music playlist: raw ogg bytes, looped round-robin on a flat sink.
	let mut playlist: Vec<Vec<u8>> = Vec::new();
	let mut next_track = 0usize;
	let music = Sink::connect_new(mixer);
	music.set_volume(MUSIC_GAIN);

	loop {
		let cmd = match rx.recv_timeout(Duration::from_millis(100)) {
			Ok(cmd) => Some(cmd),
			Err(mpsc::RecvTimeoutError::Timeout) => None,
			Err(mpsc::RecvTimeoutError::Disconnected) => return,
		};
		match cmd {
			Some(AudioCmd::Listener { pos, forward }) => {
				let f = glam::Vec3::from_array(forward);
				let right = f.cross(glam::Vec3::Y).normalize_or_zero() * EAR_HALF;
				let p = glam::Vec3::from_array(pos);
				left_ear = ((p - right) * ATT).to_array();
				right_ear = ((p + right) * ATT).to_array();
				for v in loops.values().chain(oneshots.iter()) {
					if let VoiceSink::Spatial(s) = &v.sink {
						s.set_left_ear_position(left_ear);
						s.set_right_ear_position(right_ear);
					}
				}
			}
			Some(AudioCmd::Sfx { sound, pos, gain }) => {
				let Some(src) = cache.get(&sound) else {
					continue;
				};
				let v = play_voice(
					mixer,
					src.clone(),
					pos,
					gain,
					sfx_vol,
					false,
					left_ear,
					right_ear,
				);
				if paused {
					v.pause();
				}
				oneshots.push(v);
			}
			Some(AudioCmd::LoopStart {
				id,
				sound,
				pos,
				gain,
			}) => {
				if let Some(old) = loops.remove(&id) {
					old.stop();
				}
				let Some(src) = cache.get(&sound) else {
					continue;
				};
				let v = play_voice(
					mixer,
					src.clone(),
					pos,
					gain,
					sfx_vol,
					true,
					left_ear,
					right_ear,
				);
				if paused {
					v.pause();
				}
				loops.insert(id, v);
			}
			Some(AudioCmd::LoopMove { id, pos, gain }) => {
				if let Some(v) = loops.get_mut(&id) {
					v.base_gain = gain;
					if let VoiceSink::Spatial(s) = &v.sink {
						s.set_emitter_position((glam::Vec3::from_array(pos) * ATT).to_array());
					}
					v.apply_volume(sfx_vol);
				}
			}
			Some(AudioCmd::LoopPitch { id, rate }) => {
				if let Some(v) = loops.get(&id) {
					v.set_speed(rate);
				}
			}
			Some(AudioCmd::LoopStop { id }) => {
				if let Some(v) = loops.remove(&id) {
					v.stop();
				}
			}
			Some(AudioCmd::Pause) => {
				paused = true;
				for v in loops.values().chain(oneshots.iter()) {
					v.pause();
				}
				music.pause();
			}
			Some(AudioCmd::Resume) => {
				paused = false;
				for v in loops.values().chain(oneshots.iter()) {
					v.play();
				}
				music.play();
			}
			Some(AudioCmd::Volume { music: m, sfx }) => {
				sfx_vol = sfx;
				music.set_volume(MUSIC_GAIN * m);
				// Sweep every live voice: stationary loops (waterfalls,
				// ambient bed) get no LoopMove to piggyback on.
				for v in loops.values().chain(oneshots.iter()) {
					v.apply_volume(sfx_vol);
				}
			}
			None => {}
		}
		// Housekeeping on every wakeup: GC finished one-shots, collect
		// fetched tracks, feed the playlist.
		oneshots.retain(|v| match &v.sink {
			VoiceSink::Spatial(s) => !s.empty(),
			VoiceSink::Flat(s) => !s.empty(),
		});
		while let Ok(bytes) = music_rx.try_recv() {
			playlist.push(bytes);
		}
		if !playlist.is_empty() && music.empty() && !paused {
			let track = playlist[next_track % playlist.len()].clone();
			next_track = next_track.wrapping_add(1);
			match Decoder::new(Cursor::new(track)) {
				Ok(d) => music.append(d),
				Err(e) => log::warn!("music track failed to decode: {e}"),
			}
		}
	}
}

#[allow(clippy::too_many_arguments)]
fn play_voice(
	mixer: &rodio::mixer::Mixer,
	src: Cached,
	pos: Option<[f32; 3]>,
	gain: f32,
	sfx_vol: f32,
	repeat: bool,
	left_ear: [f32; 3],
	right_ear: [f32; 3],
) -> Voice {
	let sink = match pos {
		Some(p) => {
			let s = SpatialSink::connect_new(
				mixer,
				(glam::Vec3::from_array(p) * ATT).to_array(),
				left_ear,
				right_ear,
			);
			if repeat {
				s.append(src.repeat_infinite());
			} else {
				s.append(src);
			}
			VoiceSink::Spatial(s)
		}
		None => {
			let s = Sink::connect_new(mixer);
			if repeat {
				s.append(src.repeat_infinite());
			} else {
				s.append(src);
			}
			VoiceSink::Flat(s)
		}
	};
	let v = Voice {
		sink,
		base_gain: gain,
	};
	v.apply_volume(sfx_vol);
	v
}
