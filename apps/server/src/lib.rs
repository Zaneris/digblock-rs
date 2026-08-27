//! DigBlock dedicated server, as a library so the `digblock-server` bin,
//! the `bot` bin, and the e2e tests share one implementation. Headless:
//! digblock-core + digblock-net only — no GPU, no windowing.

pub mod ban_store;
pub mod hiscores;
pub mod player_store;
/// The journal moved to `digblock-persist` (shared with client local
/// saves — one format, one VERSION); re-exported so `crate::journal::`
/// paths stay put.
pub use digblock_persist::journal;
pub mod master;
pub mod net;
pub mod sim;
pub mod spawn_store;
pub mod store;
pub mod waystone_store;

use crossbeam_channel::Receiver;
use sim::{Sim, SimEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 4433;
/// The ports the master will list a server at, mirrored from its
/// `ServerPorts` (`apps/master/Servers/ServerPorts.cs`) and pinned to it by
/// a test — the block an operator's firewall opens and the only ports the
/// master's probe will ever dial. Checked at boot so a misconfigured
/// server fails here, not with a 400 after registering.
pub const PORT_RANGE: std::ops::RangeInclusive<u16> = DEFAULT_PORT..=4499;
/// Fixed default so an unflagged restart reopens the same world journal
/// (the desktop's random-seed default would strand the saved edits).
pub const DEFAULT_SEED: f64 = 42.0;

/// Move the world's ARRIVAL PADS aside, beside whatever `journal::archive_world` just did to the
/// journal. Both files pin an eye resolved against terrain — `spawn.json` the lock and any
/// hand-picked point, `waystones.json` the resolved site per biome — and both are read back at
/// boot as final, on the standing rule that a protected region cannot have stopped being valid.
/// That rule holds against EDITS and not against regenerating the same coordinates under another
/// generator, so a world replaced without these is a fresh world wearing the old one's pads:
/// a protection sphere and a stamped structure at coordinates the new terrain never produced.
/// Archived rather than deleted, since an operator may want the old spawn's numbers back.
pub fn archive_arrival_pads(dir: &std::path::Path) -> std::io::Result<()> {
	for name in ["spawn.json", "waystones.json"] {
		let path = dir.join(name);
		if path.exists() {
			let bak = spawn_store::bak_path(&path);
			log::warn!("archiving {} to {}", path.display(), bak.display());
			std::fs::rename(&path, &bak)?;
		}
	}
	Ok(())
}

/// Run the sim loop until `running` goes false. Instant-anchored pacing;
/// when the OS oversleeps past the catch-up bound (container jitter), the
/// backlog is dropped rather than replayed.
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

#[cfg(test)]
mod tests {
	use super::*;

	/// The master refuses a port outside ITS block, and nothing links the
	/// two programs — so this reads the C# source. A drift would let a
	/// server boot with a port the master then refuses, which neither
	/// side's own tests would report.
	#[test]
	fn the_masters_port_block_mirrors_ours() {
		let src = std::fs::read_to_string(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/../master/Servers/ServerPorts.cs"
		))
		.expect("the master's ServerPorts.cs beside this workspace");
		assert!(
			src.contains(&format!("Min = {};", PORT_RANGE.start())),
			"the master's ServerPorts.Min is not our range's start"
		);
		assert!(
			src.contains(&format!("Max = {};", PORT_RANGE.end())),
			"the master's ServerPorts.Max is not our range's end"
		);
		assert!(PORT_RANGE.contains(&DEFAULT_PORT));
		// The deploy kit is what an operator reads, and it cannot derive
		// the block — so it quotes it, and this keeps the quote honest.
		for kit in ["compose.yml", "README.md"] {
			let text = std::fs::read_to_string(
				std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy"))
					.join(kit),
			)
			.expect("the deploy kit beside this workspace");
			assert!(
				text.contains(&format!("{}–{}", PORT_RANGE.start(), PORT_RANGE.end())),
				"deploy/{kit} does not name the port block"
			);
		}
		// The fleet tool validates its inventory against the block, and
		// Python cannot read this constant either.
		let fleet = std::fs::read_to_string(concat!(
			env!("CARGO_MANIFEST_DIR"),
			"/../../tools/fleet/fleet.py"
		))
		.expect("the fleet tool beside this workspace");
		assert!(
			fleet.contains(&format!(
				"PORT_BLOCK = ({}, {})",
				PORT_RANGE.start(),
				PORT_RANGE.end()
			)),
			"tools/fleet/fleet.py does not quote the port block"
		);
	}
}
