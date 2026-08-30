//! Web Worker chunk-job pool — the browser's `Executor::Workers`.
//!
//! Workers can't share the client's linear memory, so jobs cross as bytes
//! (`digblock_core::job_wire`) in transferable `ArrayBuffer`s. That costs a copy
//! each way against jobs that cost around a millisecond, so the transfer has
//! never been the interesting number. What the pool buys is the main thread.
//!
//! The pool is best-effort by construction, in both directions. Until a worker
//! reports in — and forever, if the browser refuses to spawn any — `ready()` is
//! false and the pipeline runs jobs inline. A worker that dies AFTER reporting
//! in is written off the same way: the slot stops taking work, its jobs go back
//! through `lost()`, and once no slot is left the inline path takes over again.
//! Nothing here may be a one-way door — a browser tab lives for hours.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use digblock_core::pipeline::{Job, JobDispatch, JobKey, JobResult};
use wasm_bindgen::prelude::*;

/// Jobs one worker may have outstanding.
///
/// A FEED RATE, not a fairness knob, and the whole throughput story: `top_up_workers` refills once
/// per `maintain`, so throughput caps at `DEPTH × workers × fps`. Deep enough that a worker always
/// has a backlog; the queue costs nothing but the encoded jobs sitting in it.
const DEPTH: usize = 16;

/// Sweeps a slot may go without a reply while holding work before it is written off.
///
/// Counted in SWEEPS — one per `maintain` that found the inbox dry — rather than in seconds: a
/// hidden tab throttles the workers as hard as the page, so a wall clock would declare a healthy
/// pool dead on the first frame back from the background, while a sweep counter does not advance.
const WEDGE_SWEEPS: u32 = 600;

/// Default ceiling on workers, whatever the machine reports.
///
/// The next rung up was measured and DECLINED: it fills faster and still beats the inline path on
/// p99, but it is the only arm that drops a frame, and absorbing results is pinned to the thread
/// that owns the device. `default_count` is what protects weak hardware, so re-measure there.
pub(crate) const MAX_WORKERS: usize = 2;

/// Absolute ceiling for an explicit `?workers=` override. The override
/// exists to measure past the shipped default — clamping it to the default
/// would make the A/B it was added for impossible — but not to let a URL
/// spawn an arbitrary number of wasm instances.
pub(crate) const WORKERS_HARD_MAX: usize = 8;

/// How many workers to start on this machine, before any `?workers=`
/// override. Zero on a device with barely any cores: more wasm instances,
/// each with its own linear memory, is a poor trade where there is no
/// spare core to run them on and little RAM to hold them.
pub(crate) fn default_count() -> usize {
	let cores = web_sys::window()
		.map(|w| w.navigator().hardware_concurrency() as usize)
		.unwrap_or(0);
	if cores <= 2 {
		0
	} else {
		MAX_WORKERS.min(cores - 2)
	}
}

/// One worker's reply: which slot it came from, the id it was dispatched
/// under, and the encoded result — empty when the worker could not run the
/// job at all (see the bootstrap's catch), which the pool reports as lost
/// rather than silently dropping.
type Reply = (usize, u64, Vec<u8>);
/// Shared with every worker's message handler, which is the only writer.
type Inbox = Rc<RefCell<VecDeque<Reply>>>;

struct Slot {
	worker: web_sys::Worker,
	/// Set by this worker's `{ready:true}` handshake. Per SLOT, never per
	/// pool: `submit` must not post to a worker that has not instantiated,
	/// because that message lands in a queue nothing will ever service and
	/// the job is simply gone. One booted worker does not vouch for seven
	/// others that never will be.
	ready: Rc<Cell<bool>>,
	/// Written off — `onerror`, or the wedge sweep below. Takes no more
	/// work, and whatever it was holding is handed back through `lost`.
	dead: Rc<Cell<bool>>,
	/// Posted and unanswered, in dispatch order. The key is all the
	/// pipeline needs to requeue the job (see `JobKey`), which is why the
	/// encoded payload is not retained alongside it.
	sent: VecDeque<(u64, JobKey)>,
	/// Consecutive sweeps this slot has held work without answering.
	quiet_sweeps: u32,
	// Closures are called by JS for the worker's lifetime; dropping one
	// would leave the handler pointing at freed wasm.
	_on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
	_on_error: Closure<dyn FnMut(web_sys::Event)>,
}

pub(crate) struct WorkerPool {
	slots: Vec<Slot>,
	/// Replies awaiting `poll`.
	inbox: Inbox,
	/// Jobs accepted that no result will ever come back for, awaiting
	/// `lost`.
	lost_jobs: VecDeque<JobKey>,
	next_id: u64,
	/// Replies below this id belong to a world that no longer exists.
	/// Bumping one counter is most of `reset` — ids are monotonic, so no
	/// per-job bookkeeping is needed to invalidate every job at once.
	min_valid_id: u64,
	next_slot: usize,
	blob_url: String,
	/// Interned message keys. `Reflect::set` with a fresh `&str` interns a
	/// JS string per call, and `submit` runs up to `DEPTH × workers` times
	/// a frame — the same trap `digblock-net-web`'s transport documents.
	key_id: JsValue,
	key_job: JsValue,
}

impl WorkerPool {
	/// Start `count` workers against the stamped worker module at
	/// `module_url`. `None` if the platform gave us nothing usable — the
	/// caller then simply doesn't attach a pool.
	pub(crate) fn start(module_url: &str, count: usize) -> Option<WorkerPool> {
		if count == 0 {
			return None;
		}
		let absolute = absolute_url(module_url)?;
		// The bootstrap lives in a blob rather than as a file in the bundle, so the deployed
		// bundle stays exactly one stamped pair per module. A third, unstamped `.js` would be a
		// cacheable file whose contents can drift from the module it imports — the LinkError
		// shape (see apps/web/_headers).
		let source = bootstrap_source(&absolute);
		let parts = js_sys::Array::of1(&JsValue::from_str(&source));
		let bag = web_sys::BlobPropertyBag::new();
		bag.set_type("text/javascript");
		let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag).ok()?;
		let blob_url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;

		let inbox = Inbox::default();
		let mut slots = Vec::with_capacity(count);
		for i in 0..count {
			match spawn(&blob_url, i, &inbox) {
				Some(slot) => slots.push(slot),
				// Partial success is still a pool; none at all is not.
				None => log::warn!("chunk worker {i} failed to start"),
			}
		}
		if slots.is_empty() {
			let _ = web_sys::Url::revoke_object_url(&blob_url);
			log::warn!("no chunk workers started; jobs stay on the main thread");
			return None;
		}
		log::info!("{} chunk worker(s) starting", slots.len());
		Some(WorkerPool {
			slots,
			inbox,
			lost_jobs: VecDeque::new(),
			next_id: 1,
			min_valid_id: 1,
			next_slot: 0,
			blob_url,
			key_id: JsValue::from_str("id"),
			key_job: JsValue::from_str("job"),
		})
	}

	/// Retire workers that took work and stopped answering, and hand back what a dead slot held.
	///
	/// `outstanding` only retires on a reply, so a worker that traps once holds its depth forever;
	/// with every slot so wedged `submit` refuses everything, and a refusal means "busy" rather
	/// than the UNREADY that sends the pipeline inline, so streaming would stop for the session.
	fn sweep(&mut self) {
		for (i, slot) in self.slots.iter_mut().enumerate() {
			if !slot.dead.get() {
				if slot.sent.is_empty() {
					slot.quiet_sweeps = 0;
					continue;
				}
				slot.quiet_sweeps += 1;
				if slot.quiet_sweeps < WEDGE_SWEEPS {
					continue;
				}
				log::warn!(
					"chunk worker {i} went quiet holding {} job(s); retiring it",
					slot.sent.len()
				);
				slot.dead.set(true);
			}
			// Dead, however it got there: nothing it holds is coming back.
			for (_, key) in slot.sent.drain(..) {
				self.lost_jobs.push_back(key);
			}
			slot.quiet_sweeps = 0;
		}
	}

	/// Drop `id` from its slot's outstanding list, returning what the job
	/// was. `None` for an id the slot no longer knows about — a reply that
	/// arrived after the sweep already gave up on it.
	fn retire(&mut self, slot: usize, id: u64) -> Option<JobKey> {
		let slot = self.slots.get_mut(slot)?;
		let at = slot.sent.iter().position(|(i, _)| *i == id)?;
		let (_, key) = slot.sent.remove(at)?;
		slot.quiet_sweeps = 0;
		Some(key)
	}
}

impl Drop for WorkerPool {
	fn drop(&mut self) {
		for slot in &self.slots {
			slot.worker.terminate();
		}
		let _ = web_sys::Url::revoke_object_url(&self.blob_url);
	}
}

impl JobDispatch for WorkerPool {
	fn ready(&self) -> bool {
		self.slots.iter().any(|s| s.ready.get() && !s.dead.get())
	}

	fn worker_count(&self) -> usize {
		self.slots
			.iter()
			.filter(|s| s.ready.get() && !s.dead.get())
			.count()
	}

	fn submit(&mut self, job: Job) -> Result<(), Job> {
		let n = self.slots.len();
		if n == 0 {
			return Err(job);
		}
		for i in 0..n {
			let idx = (self.next_slot + i) % n;
			let slot = &self.slots[idx];
			if !slot.ready.get() || slot.dead.get() || slot.sent.len() >= DEPTH {
				continue;
			}
			let key = job.key();
			let bytes = job.encode();
			// Copies wasm memory into a fresh JS buffer, which is then
			// transferred (not cloned) to the worker.
			let arr = js_sys::Uint8Array::from(bytes.as_slice());
			let buffer = arr.buffer();
			let msg = js_sys::Object::new();
			let id = self.next_id;
			if js_sys::Reflect::set(&msg, &self.key_id, &JsValue::from_f64(id as f64)).is_err()
				|| js_sys::Reflect::set(&msg, &self.key_job, &buffer).is_err()
			{
				return Err(job);
			}
			let transfer = js_sys::Array::of1(&buffer);
			if self.slots[idx]
				.worker
				.post_message_with_transfer(&msg, &transfer)
				.is_err()
			{
				return Err(job);
			}
			let slot = &mut self.slots[idx];
			slot.sent.push_back((id, key));
			slot.quiet_sweeps = 0;
			self.next_id += 1;
			self.next_slot = (idx + 1) % n;
			return Ok(());
		}
		Err(job)
	}

	fn poll(&mut self) -> Option<JobResult> {
		loop {
			let next = self.inbox.borrow_mut().pop_front();
			let Some((slot, id, bytes)) = next else {
				// The inbox is dry — the once-per-drain moment at which a
				// worker that stopped answering can be noticed at all.
				self.sweep();
				return None;
			};
			if id < self.min_valid_id {
				// A previous world's job. `reset` already dropped the
				// bookkeeping, so there is nothing to retire and nothing to
				// hand back — least of all a `Generated` column, which
				// carries no seed and would land as this world's terrain.
				continue;
			}
			let Some(key) = self.retire(slot, id) else {
				continue; // already given up on by the sweep
			};
			if bytes.is_empty() {
				// The worker answered but could not run the job. Nothing is
				// lost but the work — as long as we say so.
				self.lost_jobs.push_back(key);
				continue;
			}
			match JobResult::decode(&bytes) {
				Some(r) => return Some(r),
				None => {
					log::warn!("undecodable job result from chunk worker {slot}");
					self.lost_jobs.push_back(key);
					continue;
				}
			}
		}
	}

	fn lost(&mut self) -> Option<JobKey> {
		self.lost_jobs.pop_front()
	}

	fn reset(&mut self) {
		self.min_valid_id = self.next_id;
		self.inbox.borrow_mut().clear();
		// Not reported lost: the incoming pipeline's store has never heard
		// of these chunks.
		self.lost_jobs.clear();
		// Releasing the depth is the other half of `reset`. Left in place,
		// the old world's jobs would count against `DEPTH` on every slot,
		// so the new world's first `top_up_workers` would be refused
		// everywhere — while `ready()` stayed true, which is the one state
		// in which nothing streams AND nothing falls back to inline.
		for slot in &mut self.slots {
			slot.sent.clear();
			slot.quiet_sweeps = 0;
		}
	}
}

fn spawn(blob_url: &str, index: usize, inbox: &Inbox) -> Option<Slot> {
	let opts = web_sys::WorkerOptions::new();
	opts.set_type(web_sys::WorkerType::Module);
	let worker = web_sys::Worker::new_with_options(blob_url, &opts).ok()?;
	let ready = Rc::new(Cell::new(false));
	let dead = Rc::new(Cell::new(false));

	let on_message = {
		let inbox = inbox.clone();
		let ready = ready.clone();
		let key_ready = JsValue::from_str("ready");
		let key_error = JsValue::from_str("error");
		let key_id = JsValue::from_str("id");
		let key_out = JsValue::from_str("out");
		Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
			let data = e.data();
			if js_sys::Reflect::get(&data, &key_ready).is_ok_and(|v| v.is_truthy()) {
				ready.set(true);
				return;
			}
			if let Some(err) = js_sys::Reflect::get(&data, &key_error)
				.ok()
				.and_then(|v| v.as_string())
			{
				log::warn!("chunk worker {index} could not run a job: {err}");
			}
			let (Ok(id), Ok(out)) = (
				js_sys::Reflect::get(&data, &key_id),
				js_sys::Reflect::get(&data, &key_out),
			) else {
				return;
			};
			let Some(id) = id.as_f64() else {
				return;
			};
			// EVERY reply is forwarded, a failed one included: the
			// bookkeeping lives on the Rust side in `poll`, and an empty
			// buffer is exactly what tells it the job was lost rather than
			// finished. A handler that returned early here would leak the
			// slot's depth, which is how the pool used to wedge.
			let bytes = out
				.dyn_into::<js_sys::Uint8Array>()
				.map(|a| a.to_vec())
				.unwrap_or_default();
			inbox.borrow_mut().push_back((index, id as u64, bytes));
		})
	};
	worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

	let on_error = {
		let dead = dead.clone();
		Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
			// An uncaught throw in the worker — the module failed to load,
			// or the wasm is gone. Write the slot off rather than keep
			// posting into it; the next sweep hands its jobs back.
			log::warn!("chunk worker {index} errored; retiring it");
			dead.set(true);
		})
	};
	worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

	Some(Slot {
		worker,
		ready,
		dead,
		sent: VecDeque::new(),
		quiet_sweeps: 0,
		_on_message: on_message,
		_on_error: on_error,
	})
}

/// Workers load from a blob URL, which is no base for a relative import —
/// so the module reference has to be absolute before it goes in.
fn absolute_url(url: &str) -> Option<String> {
	let href = web_sys::window()?.location().href().ok()?;
	web_sys::Url::new_with_base(url, &href)
		.ok()
		.map(|u| u.href())
}

fn bootstrap_source(module_url: &str) -> String {
	// Two rules hold this together.
	//
	// `await boot` on every message rather than only the first: messages can be
	// posted before instantiation finishes, and a worker that dropped those
	// would lose chunks with no error anywhere.
	//
	// And every path answers. The handler is async, so a throw inside it — a
	// wasm trap, an allocation failure, a rejected `boot` — REJECTS ITS PROMISE
	// rather than escaping as an uncaught exception, and only an uncaught
	// exception reaches the parent's `Worker.onerror`. Without the catch the
	// reply simply never arrives: the pool's slot never retires the job, the
	// chunk is stranded, and there is no diagnostic anywhere.
	format!(
		r#"import init, {{ run_job_bytes }} from "{module_url}";
const boot = init().then(() => self.postMessage({{ ready: true }}));
self.onmessage = async (e) => {{
  const id = e.data && e.data.id;
  try {{
    await boot;
    const out = run_job_bytes(new Uint8Array(e.data.job));
    self.postMessage({{ id, out }}, [out.buffer]);
  }} catch (err) {{
    if (id !== undefined) {{
      // Empty `out` = "accepted, could not run it"; the main thread
      // requeues the chunk.
      self.postMessage({{ id, out: new Uint8Array(0), error: String(err) }});
    }}
  }}
}};
"#
	)
}
