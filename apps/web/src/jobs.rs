//! Web Worker chunk-job pool — the browser's `Executor::Workers`.
//!
//! Workers can't share the client's linear memory, so jobs cross as bytes
//! (`digblock_core::job_wire`) in transferable `ArrayBuffer`s. That costs a
//! copy each way — ~12 KB out for a mesh job, a few KB back — against jobs
//! that cost 0.1–1.2 ms, so the transfer has never been the interesting
//! number. What the pool buys is the main thread: streaming used to be paid
//! for out of frame headroom (and out of an *unbudgeted* floor batch that no
//! controller could shed), which is the one resource that vanishes on the
//! weak hardware this exists for.
//!
//! The pool is best-effort by construction. Until a worker reports in — and
//! forever, if the browser refuses to spawn any — `ready()` is false and the
//! pipeline runs jobs inline exactly as it did before this module existed.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use digblock_core::pipeline::{Job, JobDispatch, JobResult};
use wasm_bindgen::prelude::*;

/// Jobs one worker may have outstanding.
///
/// This is a FEED RATE, not a fairness knob, and it is the whole
/// throughput story. `top_up_workers` refills once per `maintain`, so a
/// worker that empties its queue mid-frame idles until the next one:
/// throughput caps at `DEPTH × workers × fps` no matter how fast the jobs
/// are. At 2 that measured 460 jobs/s on a 120 Hz browser with the pool
/// 9% utilized and 1124 columns queued — the same starvation BENCH.md's
/// second pass found natively with `bounded(workers*2)`, whose fix was
/// `workers*16`. Deep enough that a worker always has a backlog; the queue
/// costs nothing but the encoded jobs sitting in it.
const DEPTH: usize = 16;

/// Default ceiling on workers, whatever the machine reports.
///
/// Measured in Chrome, M2 Max, 120 Hz, `?bench&fly=1&frames=1500&seed=42`,
/// draw 20 — the full table is in BENCH.md:
///
/// | workers | fill | p99 | worst | over-12.5 ms |
/// |---|---|---|---|---|
/// | 0 | never (>12.5 s) | 11.10 | 11.80 | 0 |
/// | 2 | 11.91 s | 9.70 | 11.00 | 0 |
/// | 4 | 4.63 s | 10.20 | 15.50 | 4 |
///
/// **Four was measured and declined, not overlooked.** It fills 2.6×
/// faster and still beats the inline path on p99, but it is the only arm
/// that drops a frame, and this whole change exists to buy frame time
/// rather than spend it. Jobs move off-thread; ABSORBING their results —
/// store bookkeeping and `write_buffer` — is pinned to the thread that
/// owns the device and cannot follow them, which is what the 15.50 ms
/// worst is. Two takes a slower fill for a clean frame.
///
/// Note this constant is NOT what protects weak hardware — `default_count`
/// does, via `cores - 2`. A 4-core machine gets 2 whatever this says, and
/// a 1–2 core machine gets none. Raising it only ever affects machines
/// with cores to spare, so re-measure THERE (and on the fill, which is
/// what a player actually waits on) before changing it.
pub const MAX_WORKERS: usize = 2;

/// Absolute ceiling for an explicit `?workers=` override. The override
/// exists to measure past the shipped default — clamping it to the default
/// would make the A/B it was added for impossible — but not to let a URL
/// spawn an arbitrary number of wasm instances.
pub const WORKERS_HARD_MAX: usize = 8;

/// How many workers to start on this machine, before any `?workers=`
/// override. Zero on a 1–2 core device: two more wasm instances, each with
/// its own linear memory, is a poor trade where there is no spare core to
/// run them on and little RAM to hold them.
pub fn default_count() -> usize {
	let cores = web_sys::window()
		.map(|w| w.navigator().hardware_concurrency() as usize)
		.unwrap_or(0);
	if cores <= 2 {
		0
	} else {
		MAX_WORKERS.min(cores - 2)
	}
}

/// A finished job, tagged with the id it was dispatched under — the id is
/// what makes a previous world's replies droppable (see `reset`).
type Reply = (u64, Vec<u8>);
/// Shared with every worker's message handler, which is the only writer.
type Inbox = Rc<RefCell<VecDeque<Reply>>>;

struct Slot {
	worker: web_sys::Worker,
	outstanding: Rc<Cell<usize>>,
	// Closures are called by JS for the worker's lifetime; dropping one
	// would leave the handler pointing at freed wasm.
	_on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
	_on_error: Closure<dyn FnMut(web_sys::Event)>,
}

pub struct WorkerPool {
	slots: Vec<Slot>,
	/// Replies awaiting `poll`.
	inbox: Inbox,
	ready: Rc<Cell<usize>>,
	next_id: u64,
	/// Replies below this id belong to a world that no longer exists.
	/// Bumping one counter is the whole of `reset` — ids are monotonic, so
	/// no per-job bookkeeping is needed to invalidate every job at once.
	min_valid_id: u64,
	next_slot: usize,
	blob_url: String,
}

impl WorkerPool {
	/// Start `count` workers against the stamped worker module at
	/// `module_url`. `None` if the platform gave us nothing usable — the
	/// caller then simply doesn't attach a pool.
	pub fn start(module_url: &str, count: usize) -> Option<WorkerPool> {
		if count == 0 {
			return None;
		}
		let absolute = absolute_url(module_url)?;
		// The bootstrap is generated here and lives in a blob rather than
		// as a file in the bundle, so the deployed bundle stays exactly
		// one stamped pair per module. A third, unstamped `.js` would be
		// a cacheable file whose contents can drift from the module it
		// imports — the precise shape of the LinkError this project has
		// shipped twice (see apps/web/_headers).
		let source = bootstrap_source(&absolute);
		let parts = js_sys::Array::of1(&JsValue::from_str(&source));
		let bag = web_sys::BlobPropertyBag::new();
		bag.set_type("text/javascript");
		let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &bag).ok()?;
		let blob_url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;

		let inbox = Inbox::default();
		let ready = Rc::new(Cell::new(0usize));
		let mut slots = Vec::with_capacity(count);
		for i in 0..count {
			match spawn(&blob_url, i, &inbox, &ready) {
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
			ready,
			next_id: 1,
			min_valid_id: 1,
			next_slot: 0,
			blob_url,
		})
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
		self.ready.get() > 0
	}

	fn submit(&mut self, job: Job) -> Result<(), Job> {
		if self.ready.get() == 0 {
			return Err(job);
		}
		let n = self.slots.len();
		for i in 0..n {
			let idx = (self.next_slot + i) % n;
			if self.slots[idx].outstanding.get() >= DEPTH {
				continue;
			}
			let bytes = job.encode();
			// Copies wasm memory into a fresh JS buffer, which is then
			// transferred (not cloned) to the worker.
			let arr = js_sys::Uint8Array::from(bytes.as_slice());
			let buffer = arr.buffer();
			let msg = js_sys::Object::new();
			let id = self.next_id;
			if js_sys::Reflect::set(&msg, &"id".into(), &JsValue::from_f64(id as f64)).is_err()
				|| js_sys::Reflect::set(&msg, &"job".into(), &buffer).is_err()
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
			self.slots[idx]
				.outstanding
				.set(self.slots[idx].outstanding.get() + 1);
			self.next_id += 1;
			self.next_slot = (idx + 1) % n;
			return Ok(());
		}
		Err(job)
	}

	fn poll(&mut self) -> Option<JobResult> {
		loop {
			let (id, bytes) = self.inbox.borrow_mut().pop_front()?;
			if id < self.min_valid_id {
				continue; // a previous world's job
			}
			match JobResult::decode(&bytes) {
				Some(r) => return Some(r),
				None => {
					// The chunk is simply not marked done, so the pipeline
					// reschedules it; nothing is lost but the work.
					log::warn!("undecodable job result from a chunk worker");
					continue;
				}
			}
		}
	}

	fn reset(&mut self) {
		self.min_valid_id = self.next_id;
		self.inbox.borrow_mut().clear();
	}
}

fn spawn(blob_url: &str, index: usize, inbox: &Inbox, ready: &Rc<Cell<usize>>) -> Option<Slot> {
	let opts = web_sys::WorkerOptions::new();
	opts.set_type(web_sys::WorkerType::Module);
	let worker = web_sys::Worker::new_with_options(blob_url, &opts).ok()?;
	let outstanding = Rc::new(Cell::new(0usize));

	let on_message = {
		let inbox = inbox.clone();
		let ready = ready.clone();
		let outstanding = outstanding.clone();
		Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
			let data = e.data();
			if js_sys::Reflect::get(&data, &"ready".into()).is_ok_and(|v| v.is_truthy()) {
				ready.set(ready.get() + 1);
				return;
			}
			// EVERY reply retires exactly one outstanding job, including a
			// malformed one — a reply that didn't decrement would leak the
			// slot's depth and eventually wedge this worker at zero
			// capacity for the rest of the session.
			outstanding.set(outstanding.get().saturating_sub(1));
			let Ok(id) = js_sys::Reflect::get(&data, &"id".into()) else {
				return;
			};
			let Ok(out) = js_sys::Reflect::get(&data, &"out".into()) else {
				return;
			};
			let (Some(id), Ok(out)) = (id.as_f64(), out.dyn_into::<js_sys::Uint8Array>()) else {
				return;
			};
			let bytes = out.to_vec();
			if bytes.is_empty() {
				return; // the worker refused the job; it will be rescheduled
			}
			inbox.borrow_mut().push_back((id as u64, bytes));
		})
	};
	worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

	let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
		log::warn!("chunk worker {index} errored");
	});
	worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));

	Some(Slot {
		worker,
		outstanding,
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
	// `await boot` on every message rather than only the first: messages
	// can be posted before instantiation finishes, and a worker that
	// dropped those would lose chunks with no error anywhere.
	format!(
		r#"import init, {{ run_job_bytes }} from "{module_url}";
const boot = init().then(() => self.postMessage({{ ready: true }}));
self.onmessage = async (e) => {{
  await boot;
  const {{ id, job }} = e.data;
  const out = run_job_bytes(new Uint8Array(job));
  self.postMessage({{ id, out }}, [out.buffer]);
}};
"#
	)
}
