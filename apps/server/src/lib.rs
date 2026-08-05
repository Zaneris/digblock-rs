//! DigBlock dedicated server, as a library so the `digblock-server` bin,
//! the `bot` bin, and the e2e tests share one implementation. Headless:
//! digblock-core + digblock-net only — no GPU, no windowing.

pub mod ban_store;
pub mod player_store;
/// The journal moved to `digblock-persist` (shared with client local
/// saves — one format, one VERSION); re-exported so `crate::journal::`
/// paths stay put.
pub use digblock_persist::journal;
pub mod master;
pub mod net;
pub mod sim;
pub mod store;

use crossbeam_channel::Receiver;
use sim::{Sim, SimEvent};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 4433;
/// Fixed default so an unflagged restart reopens the same world journal
/// (the desktop's random-seed default would strand the saved edits).
pub const DEFAULT_SEED: f64 = 42.0;

/// Run the 60 Hz sim loop until `running` goes false. Instant-anchored
/// pacing; when the OS oversleeps more than 5 ticks (container jitter),
/// the backlog is dropped rather than replayed.
pub fn run_sim_loop(
	mut sim: Sim,
	events: Receiver<SimEvent>,
	running: Arc<AtomicBool>,
	players: Arc<AtomicUsize>,
) {
	let tick = Duration::from_secs_f64(1.0 / digblock_net::protocol::TICK_RATE as f64);
	let mut next = Instant::now();
	while running.load(Ordering::Relaxed) {
		sim.tick_once(events.try_iter().collect());
		players.store(sim.player_count(), Ordering::Relaxed);
		next += tick;
		let now = Instant::now();
		if now < next {
			std::thread::sleep(next - now);
		} else if now - next > tick * 5 {
			log::warn!(
				"sim overran by {:.1} ticks; dropping backlog",
				(now - next).as_secs_f64() * 60.0
			);
			next = now;
		}
	}
	sim.shutdown();
	log::info!("sim stopped at tick {}", sim.server_tick());
}
