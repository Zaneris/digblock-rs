//! Master-server HTTP client: registration, heartbeats, one-time join
//! token verification, and the graceful-shutdown notify. The wire contract
//! lives in apps/master (Contracts.cs) — keep the serde shapes in lockstep.
//!
//! All request functions are blocking (ureq); every caller wraps them in
//! `tokio::task::spawn_blocking`. Playing on a server is impossible without
//! a master-verified token, so there is no unregistered service mode —
//! registration retries forever and joins are refused until it lands.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Player slots: ids `0..SERVER_ID`, with `SERVER_ID` reserved.
pub const MAX_PLAYERS: u32 = digblock_net::protocol::SERVER_ID as u32;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_MIN: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct MasterConfig {
	/// Base URL, e.g. `http://127.0.0.1:5200`.
	pub url: String,
	pub server_name: String,
	/// Public host clients connect to. The server never learns its own
	/// routable address, so this must be explicit.
	pub advertise_host: String,
	/// Alternate address for the master's reachability probes when its
	/// network view differs from clients' (NAT-split dev topologies, e.g.
	/// a containerized master probing host.docker.internal while clients
	/// dial 127.0.0.1). None = probe the advertise host; production
	/// servers leave this unset.
	pub probe_host: Option<String>,
	pub advertise_port: u16,
}

#[derive(Clone)]
pub struct Registration {
	pub server_id: String,
	pub server_key: String,
}

/// Shared with every connection task plus the heartbeat and shutdown paths.
pub struct MasterState {
	pub url: String,
	registration: RwLock<Option<Registration>>,
	/// Replay backstop: the master consumes tokens on first verify; this
	/// stops a second connection racing the same token without HTTP. Only
	/// successfully verified tokens land here (never pruned — 16 bytes per
	/// join per boot).
	used_tokens: Mutex<rustc_hash::FxHashSet<[u8; 16]>>,
}

impl MasterState {
	pub fn new(url: String) -> Self {
		Self {
			url,
			registration: RwLock::new(None),
			used_tokens: Mutex::new(rustc_hash::FxHashSet::default()),
		}
	}

	pub fn registration(&self) -> Option<Registration> {
		self.registration.read().unwrap().clone()
	}

	pub fn set_registration(&self, reg: Option<Registration>) {
		*self.registration.write().unwrap() = reg;
	}

	pub fn token_already_used(&self, token: &[u8; 16]) -> bool {
		self.used_tokens.lock().unwrap().contains(token)
	}

	pub fn mark_token_used(&self, token: [u8; 16]) {
		self.used_tokens.lock().unwrap().insert(token);
	}
}

pub enum RegisterError {
	/// The master's probe-back failed: a configuration problem (wrong
	/// `--advertise-host`, UDP not forwarded), not a transient.
	Unreachable(String),
	/// Master unreachable or 5xx — transient.
	Transport(String),
	/// Master rejected the payload — terminal short of a config fix.
	Rejected(String),
}

pub enum VerifyError {
	/// Unknown, expired, consumed, or wrong-server token (or a stale key).
	Denied,
	/// Master unreachable — the caller fails closed.
	Unavailable(String),
}

// ---- wire shapes (camelCase, mirroring apps/master/Contracts.cs) ----------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegisterBody<'a> {
	name: &'a str,
	host: &'a str,
	#[serde(skip_serializing_if = "Option::is_none")]
	probe_host: Option<&'a str>,
	port: u16,
	cert_hash: String,
	players: u32,
	max_players: u32,
	protocol_version: u16,
	sim_version: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterOkBody {
	server_id: String,
	server_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterFailedBody {
	reason: String,
	detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HeartbeatBody {
	players: u32,
	max_players: u32,
}

#[derive(Serialize)]
struct VerifyBody {
	token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyOkBody {
	display_name: String,
}

// ---- blocking HTTP (callers use spawn_blocking) ---------------------------

fn agent(timeout: Duration) -> ureq::Agent {
	ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(timeout))
			.http_status_as_error(false)
			.build(),
	)
}

pub fn register(cfg: &MasterConfig, cert_hash: &[u8; 32]) -> Result<Registration, RegisterError> {
	let body = RegisterBody {
		name: &cfg.server_name,
		host: &cfg.advertise_host,
		probe_host: cfg.probe_host.as_deref(),
		port: cfg.advertise_port,
		cert_hash: hex(cert_hash),
		players: 0,
		max_players: MAX_PLAYERS,
		protocol_version: digblock_net::protocol::PROTOCOL_VERSION,
		sim_version: digblock_core::SIM_VERSION,
	};
	let mut res = agent(Duration::from_secs(10))
		.post(format!("{}/api/servers/register", cfg.url))
		.send_json(&body)
		.map_err(|e| RegisterError::Transport(e.to_string()))?;
	match res.status().as_u16() {
		200 => {
			let ok: RegisterOkBody = res
				.body_mut()
				.read_json()
				.map_err(|e| RegisterError::Transport(format!("bad register response: {e}")))?;
			Ok(Registration {
				server_id: ok.server_id,
				server_key: ok.server_key,
			})
		}
		422 => {
			let failed: RegisterFailedBody =
				res.body_mut()
					.read_json()
					.unwrap_or_else(|_| RegisterFailedBody {
						reason: "unreachable".into(),
						detail: String::new(),
					});
			Err(RegisterError::Unreachable(format!(
				"{}: {}",
				failed.reason, failed.detail
			)))
		}
		s @ 500..=599 => Err(RegisterError::Transport(format!("http {s}"))),
		s => Err(RegisterError::Rejected(format!("http {s}"))),
	}
}

/// `Ok(display_name)` consumes the token master-side; it will never verify
/// again anywhere.
pub fn verify_token(url: &str, server_key: &str, token: &[u8; 16]) -> Result<String, VerifyError> {
	let mut res = agent(Duration::from_secs(3))
		.post(format!("{url}/api/tokens/verify"))
		.header("authorization", &format!("Bearer {server_key}"))
		.send_json(&VerifyBody { token: hex(token) })
		.map_err(|e| VerifyError::Unavailable(e.to_string()))?;
	match res.status().as_u16() {
		200 => res
			.body_mut()
			.read_json::<VerifyOkBody>()
			.map(|ok| ok.display_name)
			.map_err(|e| VerifyError::Unavailable(format!("bad verify response: {e}"))),
		401 | 404 => Err(VerifyError::Denied),
		s => Err(VerifyError::Unavailable(format!("http {s}"))),
	}
}

/// `Ok(true)` = accepted; `Ok(false)` = 404, the master lost us (restart)
/// and we must re-register.
pub fn heartbeat(
	url: &str,
	reg: &Registration,
	players: u32,
	max_players: u32,
) -> Result<bool, String> {
	let res = agent(Duration::from_secs(5))
		.post(format!("{url}/api/servers/{}/heartbeat", reg.server_id))
		.header("authorization", &format!("Bearer {}", reg.server_key))
		.send_json(&HeartbeatBody {
			players,
			max_players,
		})
		.map_err(|e| e.to_string())?;
	match res.status().as_u16() {
		204 => Ok(true),
		404 => Ok(false),
		s => Err(format!("http {s}")),
	}
}

/// Best-effort graceful-shutdown notify (removes us from the list).
pub fn notify_shutdown(url: &str, reg: &Registration) {
	let result = agent(Duration::from_secs(2))
		.delete(format!("{url}/api/servers/{}", reg.server_id))
		.header("authorization", &format!("Bearer {}", reg.server_key))
		.call();
	match result {
		Ok(res) if res.status().as_u16() == 204 => log::info!("master notified of shutdown"),
		Ok(res) => log::warn!("shutdown notify: http {}", res.status()),
		Err(e) => log::warn!("shutdown notify failed: {e}"),
	}
}

// ---- async orchestration --------------------------------------------------

/// Register (retrying forever with capped backoff — joins are refused until
/// it lands), then heartbeat every 30 s; a heartbeat 404 means the master
/// restarted, so drop the registration and re-register. `cert_hash` is
/// shared with the cert-rotation task, which updates it and signals
/// `reregister` so the master hands out the new pin; the hash is re-read on
/// every registration attempt, so a rotation landing mid-retry still
/// registers the current cert.
pub async fn run(
	state: Arc<MasterState>,
	cfg: MasterConfig,
	cert_hash: Arc<RwLock<[u8; 32]>>,
	reregister: Arc<tokio::sync::Notify>,
	players: Arc<AtomicUsize>,
) {
	register_with_retry(&state, &cfg, &cert_hash).await;
	let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
	interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	interval.tick().await; // first tick fires immediately; skip it
	loop {
		tokio::select! {
			_ = reregister.notified() => {
				state.set_registration(None);
				register_with_retry(&state, &cfg, &cert_hash).await;
				continue;
			}
			_ = interval.tick() => {}
		}
		let Some(reg) = state.registration() else {
			continue;
		};
		let url = state.url.clone();
		let count = players.load(Ordering::Relaxed) as u32;
		let sent = tokio::task::spawn_blocking(move || {
			heartbeat(&url, &reg, count.min(MAX_PLAYERS), MAX_PLAYERS)
		})
		.await;
		match sent {
			Ok(Ok(true)) => {}
			Ok(Ok(false)) => {
				log::warn!("master lost our registration (restart?); re-registering");
				state.set_registration(None);
				register_with_retry(&state, &cfg, &cert_hash).await;
			}
			Ok(Err(e)) => log::warn!("heartbeat failed: {e}"),
			Err(e) => log::warn!("heartbeat task failed: {e}"),
		}
	}
}

async fn register_with_retry(
	state: &MasterState,
	cfg: &MasterConfig,
	cert_hash: &RwLock<[u8; 32]>,
) {
	let mut delay = RETRY_MIN;
	loop {
		let cfg_c = cfg.clone();
		let hash = *cert_hash.read().unwrap();
		let attempt = tokio::task::spawn_blocking(move || register(&cfg_c, &hash)).await;
		match attempt {
			Ok(Ok(reg)) => {
				log::info!(
					"registered with master {} as {} ({}:{})",
					cfg.url,
					reg.server_id,
					cfg.advertise_host,
					cfg.advertise_port
				);
				state.set_registration(Some(reg));
				return;
			}
			Ok(Err(RegisterError::Unreachable(detail))) => {
				log::error!(
					"master says we are UNREACHABLE ({detail}) — clients cannot join.\n\
					 Check that --advertise-host {} is the address the internet sees\n\
					 and that udp {} is forwarded to this machine. Retrying in {}s.",
					cfg.advertise_host,
					cfg.advertise_port,
					delay.as_secs()
				);
			}
			Ok(Err(RegisterError::Transport(e))) => {
				log::warn!("master unreachable ({e}); retrying in {}s", delay.as_secs());
			}
			Ok(Err(RegisterError::Rejected(e))) => {
				log::error!(
					"master rejected our registration ({e}); retrying in {}s",
					delay.as_secs()
				);
			}
			Err(e) => log::warn!("register task failed: {e}"),
		}
		tokio::time::sleep(delay).await;
		delay = (delay * 2).min(RETRY_MAX);
	}
}

fn hex(bytes: &[u8]) -> String {
	use std::fmt::Write;
	bytes.iter().fold(String::new(), |mut s, b| {
		let _ = write!(s, "{b:02x}");
		s
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hex_encodes_lowercase() {
		assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
	}
}
