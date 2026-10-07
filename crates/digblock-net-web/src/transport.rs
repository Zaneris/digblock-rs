//! `NetTransport` over the browser's WebTransport API. Contract mirror
//! of digblock-net-native's ChannelTransport: exactly one `Connected`
//! (after the bidi control stream and its writer exist), exactly one
//! `Disconnected` (three async producers race for it), status
//! bookkeeping in `poll`, sends fire-and-forget, reliable bytes in
//! arbitrary chunks (the netcode's FrameReader reassembles).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use digblock_net::probe::Ping;
use digblock_net::transport::{ConnStatus, NetTransport, TransportEvent};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[wasm_bindgen]
extern "C" {
	type WebTransport;
	#[wasm_bindgen(constructor, catch)]
	fn new(url: &str, options: &JsValue) -> Result<WebTransport, JsValue>;
	#[wasm_bindgen(method, getter)]
	fn ready(this: &WebTransport) -> js_sys::Promise;
	#[wasm_bindgen(method, getter)]
	fn closed(this: &WebTransport) -> js_sys::Promise;
	/// `catch`, because a browser can ship the WebTransport constructor
	/// without this method: the call then throws rather than returning a
	/// promise that rejects, and the throw would unwind the driver task
	/// (see `prop`).
	#[wasm_bindgen(method, js_name = createBidirectionalStream, catch)]
	fn create_bidirectional_stream(this: &WebTransport) -> Result<js_sys::Promise, JsValue>;
	#[wasm_bindgen(method, catch)]
	fn close(this: &WebTransport) -> Result<(), JsValue>;
	/// `catch`, because `getStats` is not merely allowed to fail — it is allowed not to EXIST. It
	/// is in the spec but unimplemented in Chrome, the only browser this client runs on, so calling
	/// it throws rather than returning a rejecting promise. Without the catch that throw unwinds
	/// out of `poll` and out of the rAF handler, which is never re-armed and the game freezes. A
	/// diagnostic the overlay renders as dashes when absent must not be able to end the session.
	#[wasm_bindgen(method, js_name = getStats, catch)]
	fn get_stats(this: &WebTransport) -> Result<js_sys::Promise, JsValue>;

	/// `WebTransportStats`. Only `smoothedRtt` is bound — the browser's own QUIC round-trip
	/// estimate, in milliseconds. Declared here rather than taken from web-sys because this crate
	/// binds the whole WebTransport surface itself, and mixing the two would mean two
	/// `WebTransport` types. The getter returns `JsValue`, not `f64`: the field is optional in the
	/// spec, so an implementation that omits it must read as absent rather than as zero.
	type WebTransportStats;
	#[wasm_bindgen(method, getter = smoothedRtt)]
	fn smoothed_rtt(this: &WebTransportStats) -> JsValue;

	type ReadableStream;
	/// `catch`, like `get_writer`: both throw on a stream that is already
	/// locked, and a throw crossing back into wasm from the driver task
	/// unwinds past every `Result` in it (see `prop`).
	#[wasm_bindgen(method, js_name = getReader, catch)]
	fn get_reader(this: &ReadableStream) -> Result<ReadableStreamDefaultReader, JsValue>;

	type ReadableStreamDefaultReader;
	#[wasm_bindgen(method)]
	fn read(this: &ReadableStreamDefaultReader) -> js_sys::Promise;

	/// A stream read's `{ done, value }`. Typed getters compile to direct
	/// property access; `Reflect::get(&res, &"done".into())` interned a
	/// FRESH JS string on every read instead — one per read, under
	/// snapshot plus reliable traffic, all of it GC pressure that surfaces
	/// as render-thread pauses.
	type ReadResult;
	#[wasm_bindgen(method, getter)]
	fn done(this: &ReadResult) -> JsValue;
	#[wasm_bindgen(method, getter)]
	fn value(this: &ReadResult) -> JsValue;

	type WritableStream;
	#[wasm_bindgen(method, js_name = getWriter, catch)]
	fn get_writer(this: &WritableStream) -> Result<WritableStreamDefaultWriter, JsValue>;

	type WritableStreamDefaultWriter;
	#[wasm_bindgen(method)]
	fn write(this: &WritableStreamDefaultWriter, chunk: &JsValue) -> js_sys::Promise;
	/// Signal end-of-stream. The returned promise settles once the
	/// browser has flushed, which on an unloading page nobody is around
	/// to await — but ASKING is what gives the pending writes a chance,
	/// and it is the strongest thing a page has.
	#[wasm_bindgen(method)]
	fn close(this: &WritableStreamDefaultWriter) -> js_sys::Promise;
	/// Room left before the stream's high-water mark; negative means the
	/// browser is queueing faster than the connection drains. `null` once
	/// the stream errors, and the getter throws on a released writer.
	#[wasm_bindgen(method, getter, js_name = desiredSize, catch)]
	fn desired_size(this: &WritableStreamDefaultWriter) -> Result<JsValue, JsValue>;
}

struct Shared {
	events: VecDeque<TransportEvent>,
	connected_sent: bool,
	disconnected_sent: bool,
	/// A stream ending, held back to see whether the SESSION is about to
	/// account for itself. See [`stream_failed`].
	stream_failure: Option<String>,
}

impl Shared {
	/// The session's own account of the end, which always wins: it is the
	/// one that says whether the server closed us, and with what.
	fn push_disconnected(&mut self, reason: String) {
		if !self.disconnected_sent {
			self.disconnected_sent = true;
			self.stream_failure = None;
			self.events
				.push_back(TransportEvent::Disconnected { reason });
		}
	}
}

type SharedRef = Rc<RefCell<Shared>>;
type WriterSlot = Rc<RefCell<Option<WritableStreamDefaultWriter>>>;

pub struct WebTransportClient {
	shared: SharedRef,
	status: ConnStatus,
	/// The WebTransport object (as a plain handle — the typed wrapper
	/// moved into the driver task). None only when the constructor threw.
	transport: Option<JsValue>,
	/// None until the bidi control stream opens.
	reliable_writer: WriterSlot,
	datagram_writer: WriterSlot,
	/// This browser's WebTransport has no usable datagram channel, so the
	/// netcode must carry its unreliable traffic on the reliable stream.
	/// Set by the driver BEFORE it reports `Connected`, because the answer
	/// rides the Hello that Connected triggers.
	no_datagrams: Rc<Cell<bool>>,
	/// Datagrams dropped at the writer's high-water mark; the first one
	/// logs, the rest are silent.
	dropped_datagrams: u32,
	/// The browser's own QUIC round trip in ms, refreshed by `poll`.
	/// `getStats()` returns a Promise, so it cannot be read where
	/// `wire_rtt_ms` is called — the value is sampled asynchronously and
	/// parked here.
	wire_rtt: Rc<Cell<Option<f32>>>,
	/// Polls since the last `getStats()` was kicked off.
	rtt_age: u32,
	/// Latched false the first time `getStats()` throws — i.e. on every
	/// browser that does not implement it, which today is all of them. One
	/// failed call is the feature detection; without the latch the same
	/// TypeError would be caught, logged and discarded twice a second for
	/// the life of the session.
	stats_supported: bool,
}

/// Frames between `getStats()` samples. A smoothed RTT does not move fast
/// enough to be worth a promise every frame.
const RTT_SAMPLE_FRAMES: u32 = 30;

thread_local! {
	/// Swallows write-promise rejections (lossy by contract; without a catch every dropped datagram
	/// logs an unhandled-rejection warning). Module-lifetime, deliberately NOT owned by the client:
	/// `close()` rejects every in-flight write, and those rejections are microtasks that fire AFTER
	/// the wasm call returns, by which point a client-owned closure has been dropped along with the
	/// struct — wasm-bindgen's shim then throws from inside a catch handler on every disconnect.
	static NOOP_CATCH: Closure<dyn FnMut(JsValue)> = Closure::new(|_: JsValue| {});
}

fn js_reason(v: &JsValue) -> String {
	if let Some(s) = v.as_string() {
		return s;
	}
	let text = js_sys::Object::try_from(v)
		.map(|o| String::from(o.to_string()))
		.unwrap_or_else(|| format!("{v:?}"));
	// A `WebTransportError` carries the two things its message does not, and its message is
	// routinely empty: WHERE it came from — the session or one stream — and, for a stream the peer
	// reset, the code it was reset with. `Error.toString()` on an empty message is just the type
	// name, so without this every failure on this transport reads as the same three words.
	let Some(source) = prop(v, "source").and_then(|s| s.as_string()) else {
		return text;
	};
	match prop(v, "streamErrorCode").and_then(|c| c.as_f64()) {
		Some(code) => format!("{text} [{source}, code {code}]"),
		None => format!("{text} [{source}]"),
	}
}

/// The peer's own account of a clean close, out of `WebTransportCloseInfo`.
/// A session that ends politely leaves no other trace on the client, so
/// the code and reason it carries are the only evidence there is.
fn close_info(info: &JsValue) -> String {
	let code = prop(info, "closeCode").and_then(|v| v.as_f64());
	let why = prop(info, "reason")
		.and_then(|v| v.as_string())
		.filter(|s| !s.is_empty());
	match (code, why) {
		(_, Some(why)) => format!("closed by server: {why}"),
		(Some(code), None) if code != 0.0 => format!("closed by server (code {code})"),
		_ => "closed by server".into(),
	}
}

/// Run `f` after `ms`, or not at all on a page with no window.
fn set_timeout(ms: i32, f: impl FnOnce() + 'static) {
	let fired = Closure::once_into_js(f);
	if let Some(w) = web_sys::window() {
		let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(fired.unchecked_ref(), ms);
	}
}

/// A property that is present and not nullish, or `None`. Every hop through the WebTransport
/// object graph goes through this rather than a typed getter, because a browser can implement the
/// API in PARTS: the session can be there while `datagrams` is `undefined`, and reading `.writable`
/// off `undefined` throws. A throw inside the driver task lands in no `Result` — it unwinds out of
/// wasm as an unhandled rejection, and the connection just expires at the client's connect timeout.
fn prop(target: &JsValue, name: &str) -> Option<JsValue> {
	js_sys::Reflect::get(target, &JsValue::from_str(name))
		.ok()
		.filter(|v| !v.is_undefined() && !v.is_null())
}

/// The writable half of a duplex (a bidirectional stream, or the datagram
/// pair), as the writer the driver needs. `what` names the duplex for the
/// message a player sees.
fn writer_of(duplex: &JsValue, what: &str) -> Result<WritableStreamDefaultWriter, String> {
	prop(duplex, "writable")
		.ok_or_else(|| format!("{what}: no writable side"))?
		.unchecked_ref::<WritableStream>()
		.get_writer()
		.map_err(|e| format!("{what} writer: {}", js_reason(&e)))
}

/// The readable half of the same, as a reader for [`read_loop`].
fn reader_of(duplex: &JsValue, what: &str) -> Result<ReadableStreamDefaultReader, String> {
	prop(duplex, "readable")
		.ok_or_else(|| format!("{what}: no readable side"))?
		.unchecked_ref::<ReadableStream>()
		.get_reader()
		.map_err(|e| format!("{what} reader: {}", js_reason(&e)))
}

/// End the connection with a reason, logged as well as reported: the
/// console is the only place a stage that failed BEFORE the menu could
/// show anything leaves a trace.
fn fail(shared: &SharedRef, reason: String) {
	log::error!("webtransport: {reason}");
	shared.borrow_mut().push_disconnected(reason);
}

/// How long a stream's account of the end waits for the session's. A connection going down settles
/// both at once, and the read is already in flight, so it wins every time. This is only long
/// enough for the `closed` promise to land in a later microtask, and is imperceptible.
const SESSION_GRACE_MS: i32 = 250;

/// One reader loop ending — reported only if the SESSION does not account for itself first. When a
/// QUIC connection goes away BOTH things happen: the pending read rejects and `closed` settles.
/// The read always gets there first, so the player is shown a type name with an empty message in
/// place of the session's own story, the one that says whether the server dropped us and with what
/// code. A stream ending is right only when the session outlives it; a beat tells the two apart.
fn stream_failed(shared: &SharedRef, reason: String) {
	{
		let mut s = shared.borrow_mut();
		if s.disconnected_sent || s.stream_failure.is_some() {
			return;
		}
		log::warn!("webtransport: {reason}");
		s.stream_failure = Some(reason);
	}
	let shared = shared.clone();
	set_timeout(SESSION_GRACE_MS, move || {
		let mut s = shared.borrow_mut();
		if let Some(reason) = s.stream_failure.take() {
			s.push_disconnected(reason);
		}
	});
}

/// How far the driver has got, in words a player can read. The stall
/// watchdog is its only reader.
type Stage = Rc<Cell<&'static str>>;

/// Report a step that never finished, before the client's own connect deadline expires and reports
/// nothing in particular. A rejection names itself; a promise that simply never settles does not,
/// and the two are indistinguishable from above — both end as `CONNECT_TIMEOUT_SECS` of silence.
/// This fires at a fraction of that deadline so the stage that was pending is what the player
/// sees, which on a phone is the only diagnosis available at all.
fn watch_for_stall(shared: &SharedRef, stage: &Stage) {
	/// Of the client's connect deadline. Well short of it, so the two
	/// cannot race for which message a player gets.
	const OF_CONNECT_DEADLINE: f32 = 0.7;
	let ms = (digblock_net::protocol::CONNECT_TIMEOUT_SECS * 1000.0 * OF_CONNECT_DEADLINE) as i32;
	let (shared, stage) = (shared.clone(), stage.clone());
	set_timeout(ms, move || {
		let idle = {
			let s = shared.borrow();
			!s.connected_sent && !s.disconnected_sent
		};
		if idle {
			fail(&shared, format!("timed out {}", stage.get()));
		}
	});
}

/// `None` asks for nothing beyond the browser's own CA validation: a server with a real
/// certificate, which is the only kind a browser can reach without a master handing it a pin.
fn options(cert_hash: Option<[u8; 32]>) -> JsValue {
	let opts = js_sys::Object::new();
	let Some(cert_hash) = cert_hash else {
		return opts.into();
	};
	let hash = js_sys::Object::new();
	js_sys::Reflect::set(&hash, &"algorithm".into(), &"sha-256".into()).expect("set");
	js_sys::Reflect::set(
		&hash,
		&"value".into(),
		&js_sys::Uint8Array::from(&cert_hash[..]).into(),
	)
	.expect("set");
	js_sys::Reflect::set(
		&opts,
		&"serverCertificateHashes".into(),
		&js_sys::Array::of1(&hash),
	)
	.expect("set");
	opts.into()
}

/// One reader loop; `datagram` picks the event kind. A `done` result or
/// a rejection ends the connection (guarded: only the first reporter's
/// reason survives).
async fn read_loop(reader: ReadableStreamDefaultReader, shared: SharedRef, datagram: bool) {
	loop {
		let res = match JsFuture::from(reader.read()).await {
			Ok(v) => v.unchecked_into::<ReadResult>(),
			Err(e) => {
				return stream_failed(&shared, format!("stream read: {}", js_reason(&e)));
			}
		};
		// Anything but an explicit `false` ends the connection — a result
		// without a usable `done` is not a result we can keep reading.
		if !matches!(res.done().as_bool(), Some(false)) {
			return stream_failed(&shared, "stream closed by server".into());
		}
		let value = res.value();
		if value.is_undefined() || value.is_null() {
			continue;
		}
		let bytes = value.unchecked_into::<js_sys::Uint8Array>().to_vec();
		shared.borrow_mut().events.push_back(if datagram {
			TransportEvent::Datagram(bytes)
		} else {
			TransportEvent::Reliable(bytes)
		});
	}
}

/// Connect with the server's pinned certificate hash (a self-signed server), or `None` for a
/// CA-certified one. Returns immediately with `Connecting`; `url` is a WebTransport URL.
/// `force_no_datagrams` pretends this browser has no datagram channel (`?nodatagrams`), because
/// otherwise the reliable-stream fallback could only be exercised on the one browser that needs it
/// and is hardest to attach a console to — a path with no way to run on a dev machine rots.
pub fn connect(
	url: &str,
	cert_hash: Option<[u8; 32]>,
	force_no_datagrams: bool,
) -> WebTransportClient {
	let shared: SharedRef = Rc::new(RefCell::new(Shared {
		events: VecDeque::new(),
		connected_sent: false,
		disconnected_sent: false,
		stream_failure: None,
	}));
	let reliable_writer: WriterSlot = Rc::new(RefCell::new(None));
	let datagram_writer: WriterSlot = Rc::new(RefCell::new(None));
	let no_datagrams: Rc<Cell<bool>> = Rc::new(Cell::new(force_no_datagrams));

	let transport = match WebTransport::new(url, &options(cert_hash)) {
		Ok(t) => t,
		Err(e) => {
			shared
				.borrow_mut()
				.push_disconnected(format!("endpoint: {}", js_reason(&e)));
			return WebTransportClient {
				shared,
				status: ConnStatus::Connecting,
				transport: None,
				reliable_writer,
				datagram_writer,
				no_datagrams,
				dropped_datagrams: 0,
				wire_rtt: Rc::new(Cell::new(None)),
				rtt_age: 0,
				stats_supported: true,
			};
		}
	};

	let transport_handle: JsValue = AsRef::<JsValue>::as_ref(&transport).clone();

	// Driver: ready → control stream → the single Connected → datagrams, then park on `closed`
	// while the two read loops run. Every step is fallible and says which one it was: a browser
	// with PARTIAL WebTransport support gives out somewhere in here, and until it reports itself
	// the only symptom is the client's connect timeout, with a server log showing a session that
	// opened and never spoke.
	let stage: Stage = Rc::new(Cell::new("opening the session"));
	watch_for_stall(&shared, &stage);
	{
		let shared = shared.clone();
		let reliable_writer = reliable_writer.clone();
		let datagram_writer = datagram_writer.clone();
		let no_datagrams = no_datagrams.clone();
		spawn_local(async move {
			if let Err(e) = JsFuture::from(transport.ready()).await {
				return fail(&shared, format!("connect: {}", js_reason(&e)));
			}
			stage.set("opening the control stream");
			let bi = match transport.create_bidirectional_stream() {
				Ok(p) => match JsFuture::from(p).await {
					Ok(v) => v,
					Err(e) => return fail(&shared, format!("open stream: {}", js_reason(&e))),
				},
				Err(e) => return fail(&shared, format!("open stream: {}", js_reason(&e))),
			};
			let control = "control stream";
			let writer = match writer_of(&bi, control) {
				Ok(w) => w,
				Err(e) => return fail(&shared, e),
			};
			let reader = match reader_of(&bi, control) {
				Ok(r) => r,
				Err(e) => return fail(&shared, e),
			};
			// Datagrams BEFORE Connected, because the answer rides the Hello that Connected
			// triggers: a client with no datagram channel must announce it on the first frame
			// it sends, and a probe landing a tick later is a tick too late. Absence is a fact
			// about this browser, not a failure — iOS Safari ships `WebTransport.datagrams` with
			// no writable side — so it is recorded rather than reported.
			let datagrams = "datagrams";
			let pair = (!no_datagrams.get())
				.then(|| prop(AsRef::<JsValue>::as_ref(&transport), "datagrams"))
				.flatten();
			let halves = pair
				.as_ref()
				.map(|p| (writer_of(p, datagrams), reader_of(p, datagrams)));
			match halves {
				Some((Ok(w), Ok(r))) => {
					*datagram_writer.borrow_mut() = Some(w);
					spawn_local(read_loop(r, shared.clone(), true));
				}
				// Either half missing takes both down: the fallback is a
				// pair of directions, and a client receiving snapshots on
				// one channel while sending inputs on another would leave
				// the server guessing which it should answer on.
				Some((w, r)) => {
					let why = w.err().or_else(|| r.err()).unwrap_or_default();
					log::warn!("webtransport: {why} — unreliable traffic rides the control stream");
					no_datagrams.set(true);
				}
				// Absent, or refused by `?nodatagrams`.
				None => {
					log::warn!(
						"webtransport: no datagrams — unreliable traffic rides the control stream"
					);
					no_datagrams.set(true);
				}
			}
			// Connected only now: the handshake rides the reliable stream
			// alone, so nothing above this line may be able to stop the
			// client reaching a state where it can report what went wrong.
			*reliable_writer.borrow_mut() = Some(writer);
			{
				let mut s = shared.borrow_mut();
				if !s.connected_sent {
					s.connected_sent = true;
					s.events.push_back(TransportEvent::Connected);
				}
			}
			spawn_local(read_loop(reader, shared.clone(), false));

			match JsFuture::from(transport.closed()).await {
				Ok(info) => shared.borrow_mut().push_disconnected(close_info(&info)),
				Err(e) => shared
					.borrow_mut()
					.push_disconnected(format!("connection: {}", js_reason(&e))),
			}
		});
	}

	WebTransportClient {
		shared,
		status: ConnStatus::Connecting,
		transport: Some(transport_handle),
		reliable_writer,
		datagram_writer,
		no_datagrams,
		dropped_datagrams: 0,
		wire_rtt: Rc::new(Cell::new(None)),
		rtt_age: 0,
		stats_supported: true,
	}
}

impl WebTransportClient {
	fn write_to(&self, slot: &WriterSlot, bytes: &[u8]) {
		if let Some(w) = &*slot.borrow() {
			let arr: JsValue = js_sys::Uint8Array::from(bytes).into();
			NOOP_CATCH.with(|c| {
				let _ = w.write(&arr).catch(c);
			});
		}
	}

	/// Is the datagram writer's queue past its high-water mark? No signal
	/// (no writer yet, an errored stream, a released writer) reads as
	/// "send it" — this gate only ever suppresses a measured backlog.
	fn datagram_backed_up(&self) -> bool {
		let slot = self.datagram_writer.borrow();
		let Some(w) = &*slot else { return false };
		matches!(w.desired_size(), Ok(v) if v.as_f64().is_some_and(|d| d < 0.0))
	}
}

impl NetTransport for WebTransportClient {
	fn send_datagram(&mut self, bytes: &[u8]) {
		// Best-effort by contract. A stalled connection otherwise piles
		// up JS-side writes invisibly — nothing above this layer can see
		// the browser's queue — so drop at the writer's own high-water
		// mark instead. Input datagrams carry a redundancy window, so a
		// dropped one costs nothing a lost packet wouldn't.
		if self.datagram_backed_up() {
			self.dropped_datagrams += 1;
			if self.dropped_datagrams == 1 {
				log::warn!("datagram writer past its high-water mark: dropping sends");
			}
			return;
		}
		self.write_to(&self.datagram_writer, bytes);
	}

	fn send_reliable(&mut self, bytes: &[u8]) {
		// Never dropped, backed up or not: the reliable stream is the
		// edit/handshake channel and has no redundancy to fall back on.
		// The browser buffers internally, matching the native driver's
		// unbounded channel.
		self.write_to(&self.reliable_writer, bytes);
	}

	fn poll(&mut self, out: &mut Vec<TransportEvent>) {
		{
			let mut shared = self.shared.borrow_mut();
			for ev in shared.events.drain(..) {
				match &ev {
					TransportEvent::Connected => self.status = ConnStatus::Connected,
					TransportEvent::Disconnected { .. } => self.status = ConnStatus::Closed,
					_ => {}
				}
				out.push(ev);
			}
		}
		self.sample_rtt();
	}

	/// Ask the browser to flush the reliable stream and end it. No waiting, because a page cannot
	/// wait: `close()` hands back a promise, and on the `pagehide` path there is no later task for
	/// it to resolve in. Ordering is still on our side — a WritableStream close is queued behind
	/// the writes already in it, so the browser flushes them together if it flushes at all.
	/// Strictly weaker than the native close; the server's idle timeout remains the backstop.
	fn close(&mut self) {
		if self.status == ConnStatus::Closed {
			return;
		}
		if let Some(w) = &*self.reliable_writer.borrow() {
			NOOP_CATCH.with(|c| {
				let _ = w.close().catch(c);
			});
		}
		self.status = ConnStatus::Closed;
	}

	fn status(&self) -> ConnStatus {
		self.status
	}

	fn wire_rtt_ms(&self) -> Option<f32> {
		self.wire_rtt.get()
	}

	fn datagrams_available(&self) -> bool {
		!self.no_datagrams.get()
	}
}

impl WebTransportClient {
	/// Kick off a `getStats()` every `RTT_SAMPLE_FRAMES` polls and park
	/// `smoothedRtt` in the cell. Fire-and-forget: the promise resolves on a
	/// later microtask, so this frame reads the previous sample. That is
	/// fine for a smoothed RTT and it is the only shape available — the
	/// stats are async and `wire_rtt_ms` is not.
	fn sample_rtt(&mut self) {
		if self.status != ConnStatus::Connected {
			self.wire_rtt.set(None);
			return;
		}
		if !self.stats_supported {
			return;
		}
		self.rtt_age += 1;
		if self.rtt_age < RTT_SAMPLE_FRAMES {
			return;
		}
		self.rtt_age = 0;
		let Some(t) = &self.transport else { return };
		let promise = match t.unchecked_ref::<WebTransport>().get_stats() {
			Ok(p) => p,
			Err(e) => {
				// Not an error condition: this browser has no `getStats`.
				// `wire_rtt_ms` stays None for the session and the overlay
				// shows dashes, which is exactly what it renders for a
				// transport with no way to measure the wire.
				self.stats_supported = false;
				self.wire_rtt.set(None);
				log::info!(
					"WebTransport.getStats() unavailable ({}): ping reads as unknown",
					js_reason(&e)
				);
				return;
			}
		};
		let slot = self.wire_rtt.clone();
		spawn_local(async move {
			let rtt = JsFuture::from(promise)
				.await
				.ok()
				.and_then(|s| s.dyn_into::<WebTransportStats>().ok())
				.and_then(|s| s.smoothed_rtt().as_f64())
				.filter(|v| v.is_finite() && *v >= 0.0);
			slot.set(rtt.map(|v| v as f32));
		});
	}
}

/// Open a throwaway WebTransport session, read the browser's own QUIC round-trip estimate, close —
/// the browser mirror of digblock-net-native's `probe`, and the same measurement it makes. Calls
/// `done` with `Some(ping)` on success, `None` on failure or timeout. Chrome does not implement
/// `getStats` (still absent in 151), so there the row carries `Ping::from_handshake` — the opening
/// time divided by the round trips it is made of, and marked as the estimate it is.
pub fn probe(
	url: &str,
	cert_hash: Option<[u8; 32]>,
	timeout_ms: u32,
	done: impl FnOnce(Option<Ping>) + 'static,
) {
	let transport = match WebTransport::new(url, &options(cert_hash)) {
		Ok(t) => t,
		Err(_) => {
			done(None);
			return;
		}
	};
	spawn_local(async move {
		let start = web_time::Instant::now();
		let raced = js_sys::Promise::race(&js_sys::Array::of2(
			&transport.ready(),
			&timeout_sentinel(timeout_ms),
		));
		let ms = match JsFuture::from(raced).await {
			// The sentinel string means the timeout won the race.
			Ok(v) if v.as_string().as_deref() == Some("timeout") => None,
			Ok(_) => {
				// Handshake time is still the reachability answer; it is just
				// not the latency one. The estimate is made of it when the
				// browser omits `smoothedRtt`, which the spec permits.
				let handshake = start.elapsed().as_millis() as u32;
				// `get_stats` is `catch`-bound because on Chrome the method
				// does not exist and the call THROWS. Here that throw would
				// only reject this task's promise, but the same absence has
				// to read as "no measurement" on both paths, and the
				// fallback below is already the answer for it.
				let rtt = match transport.get_stats() {
					Ok(p) => JsFuture::from(p)
						.await
						.ok()
						.and_then(|s| s.dyn_into::<WebTransportStats>().ok())
						.and_then(|s| s.smoothed_rtt().as_f64())
						.filter(|v| v.is_finite() && *v > 0.0)
						.map(|v| v as u32),
					Err(_) => None,
				};
				Some(
					rtt.filter(|r| *r <= handshake)
						.map_or_else(|| Ping::from_handshake(handshake), Ping::measured),
				)
			}
			Err(_) => None,
		};
		let _ = transport.close();
		done(ms);
	});
}

/// A promise that resolves to the string "timeout" after `ms`.
fn timeout_sentinel(ms: u32) -> js_sys::Promise {
	js_sys::Promise::new(&mut |resolve, _| {
		web_sys::window()
			.expect("window")
			.set_timeout_with_callback_and_timeout_and_arguments_1(
				&resolve,
				ms as i32,
				&JsValue::from_str("timeout"),
			)
			.expect("setTimeout");
	})
}

impl Drop for WebTransportClient {
	fn drop(&mut self) {
		// Menu-cancel / engine-disconnect path: closing rejects the
		// pending read promises, which unwinds the loops. Ignore "already
		// closed" errors.
		if let Some(t) = &self.transport {
			let _ = t.unchecked_ref::<WebTransport>().close();
		}
	}
}
