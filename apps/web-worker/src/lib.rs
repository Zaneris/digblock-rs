//! The chunk-job worker — the browser's answer to `chunk-worker-N` on
//! native. One exported function, run inside a Web Worker.
//!
//! This is a separate wasm module from `digblock-web` for two reasons, and
//! both matter more than the duplication costs:
//!
//! - The main module's `#[wasm_bindgen(start)]` reaches for
//!   `web_sys::window()`, which does not exist in a worker. Instantiating
//!   it here would panic before the first job.
//! - Every worker instantiates its own copy with its own linear memory.
//!   Core-only keeps that small, and the devices this pool exists to help
//!   are precisely the ones with the least RAM to spare.
//!
//! There is no `start` here at all: instantiating this module does
//! nothing, and the pool's bootstrap decides when the first job runs.
#![cfg(target_arch = "wasm32")]

use digblock_core::pipeline::{Job, run_job};
use wasm_bindgen::prelude::*;

/// Decode a job, run it, encode the result — the whole worker.
///
/// A message that doesn't decode answers with an empty buffer rather than
/// trapping: the wasm here has no way to report a fault except by dying,
/// and a dead worker is a pool that silently loses a share of its
/// throughput for the rest of the session. The main thread reads an empty
/// reply as "accepted, could not run it", hands the job back to the
/// pipeline through `JobDispatch::lost`, and the chunk is requeued — the
/// bootstrap's catch answers this way for a trap too, so every accepted
/// job produces exactly one reply.
#[wasm_bindgen]
pub fn run_job_bytes(input: &[u8]) -> Vec<u8> {
	// Cheap after the first call, and the alternative on this platform is
	// an opaque `unreachable executed` with no frame behind it.
	console_error_panic_hook::set_once();
	match Job::decode(input) {
		Some(job) => run_job(job).encode(),
		None => Vec::new(),
	}
}
