//! Master-server HTTP client: registration, heartbeats, one-time join
//! token verification, the skill report behind the hiscores board, and the
//! graceful-shutdown notify. The wire contract lives in apps/master
//! (Contracts.cs) — keep the serde shapes in lockstep.
//!
//! All request functions are blocking (ureq); every caller wraps them in
//! `tokio::task::spawn_blocking`. Playing on a server is impossible without
//! a master-verified token, so there is no unregistered service mode —
//! registration retries forever and joins are refused until it lands.

use crate::hiscores::SkillOutbox;
use digblock_core::skills::SkillSet;
use digblock_net::protocol::AccountId;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Player slots: ids `0..SERVER_ID`, with `SERVER_ID` reserved.
pub const MAX_PLAYERS: u32 = digblock_net::protocol::SERVER_ID as u32;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Rows per skill report, the master's own cap (`HiscoreService.MaxRowsPerReport`) — a
/// larger batch is refused whole, so the outbox drains in slices of this.
const REPORT_BATCH: usize = 500;
const RETRY_MIN: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct MasterConfig {
	/// Base URL, e.g. `http://127.0.0.1:5200`.
	pub url: String,
	pub server_name: String,
	/// Public host clients connect to. None (the default) advertises the address the master
	/// observes making the registration — right for the common port-forwarded home host, since
	/// players must reach the same public IP the registration left from. Set to override with a
	/// DNS name, or when the observed egress is not the game traffic's ingress (multi-homed
	/// hosts, split routing).
	pub advertise_host: Option<String>,
	pub advertise_port: u16,
	/// Our registration key for this address, if we have registered before. The master remembers
	/// which key holds a `host:port` and refuses a registration from anyone else while something
	/// is live there, so this is what makes a restart a re-registration rather than a takeover
	/// attempt. Persisted beside the world data ([`KEY_FILE`]); `None` on a first run, which is
	/// when the master mints one.
	pub server_key: Option<String>,
}
// probe_host is gone from this struct as well as the wire. The master
// redirects its own probes now (Master:Dev:ProbeHostOverride) — letting the
// registrant choose the only address ever dialed is what that closed.

#[derive(Clone)]
pub struct Registration {
	pub server_id: String,
	pub server_key: String,
	/// The host the master actually listed us under — the configured
	/// advertise host, or the observed registration address when none
	/// was given.
	pub observed_host: String,
	/// Whether the master trusts this address to report skill totals.
	/// Decided master-side from its own list; an untrusted server never
	/// sends a report, and one that did would be refused.
	pub reports_skills: bool,
}

/// Shared with every connection task plus the heartbeat and shutdown paths.
pub struct MasterState {
	pub url: String,
	registration: RwLock<Option<Registration>>,
	/// Replay backstop: the master consumes tokens on first verify; this
	/// stops a second connection racing the same token without HTTP. Only
	/// successfully verified tokens land here (never pruned — one token's
	/// worth of bytes per join per boot).
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

#[derive(Debug)]
pub enum RegisterError {
	/// The master's probe-back failed: a configuration problem, not a transient.
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
	/// None → the master fills in the observed remote address.
	#[serde(skip_serializing_if = "Option::is_none")]
	host: Option<&'a str>,
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
	observed_host: String,
	/// Absent on a master predating the board, which reads as untrusted.
	#[serde(default)]
	reports_skills: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillReportBody {
	players: Vec<SkillReportRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillReportRow {
	account_id: String,
	skills: Vec<SkillXpBody>,
}

#[derive(Serialize, Deserialize)]
struct SkillXpBody {
	skill: u8,
	/// The master stores a signed 64-bit total; a saturated `u64` would
	/// fail its decode and take the whole batch with it.
	xp: i64,
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
	/// The master's account id for this player, as a hyphenated UUID. This
	/// is the identity everything server-side keys on — inventory, skills,
	/// admin, bans — and it arrives HERE rather than in the client's Hello
	/// precisely so the client cannot choose it.
	account_id: String,
	/// The board's copy of the account's totals; absent on a master
	/// predating the board, which reads as nothing to fold in.
	#[serde(default)]
	skills: Vec<SkillXpBody>,
}

/// What a verified token vouches for.
pub struct VerifiedJoin {
	pub display_name: String,
	pub account: AccountId,
	/// The account's totals as the board holds them — what this server
	/// folds into its own record at join, unless it keeps skills local.
	pub skills: SkillSet,
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
		host: cfg.advertise_host.as_deref(),
		port: cfg.advertise_port,
		cert_hash: hex(cert_hash),
		players: 0,
		max_players: MAX_PLAYERS,
		protocol_version: digblock_net::protocol::PROTOCOL_VERSION,
		sim_version: digblock_core::SIM_VERSION,
	};
	let mut req = agent(Duration::from_secs(10)).post(format!("{}/api/servers/register", cfg.url));
	// Present our key if we hold one. Without it the master treats this as
	// a first claim on the address — fine on a genuinely fresh install, and
	// refused while another server is live there, which is the point.
	if let Some(key) = &cfg.server_key {
		req = req.header("authorization", &format!("Bearer {key}"));
	}
	let mut res = req
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
				observed_host: ok.observed_host,
				reports_skills: ok.reports_skills,
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

/// A successful verify consumes the token master-side; it will never verify
/// again anywhere.
pub fn verify_token(
	url: &str,
	server_key: &str,
	token: &[u8; 16],
) -> Result<VerifiedJoin, VerifyError> {
	let mut res = agent(Duration::from_secs(3))
		.post(format!("{url}/api/tokens/verify"))
		.header("authorization", &format!("Bearer {server_key}"))
		.send_json(&VerifyBody { token: hex(token) })
		.map_err(|e| VerifyError::Unavailable(e.to_string()))?;
	match res.status().as_u16() {
		200 => {
			let ok: VerifyOkBody = res
				.body_mut()
				.read_json()
				.map_err(|e| VerifyError::Unavailable(format!("bad verify response: {e}")))?;
			// An unparseable account id fails the join rather than falling
			// back to anonymous: the whole point of this answer is that the
			// identity came from the master, and "we could not read it" is
			// not a licence to invent one.
			let account = AccountId::parse(&ok.account_id).ok_or_else(|| {
				VerifyError::Unavailable(format!("master sent a bad account id: {}", ok.account_id))
			})?;
			let pairs: Vec<(u8, u64)> = ok
				.skills
				.iter()
				.map(|s| (s.skill, s.xp.max(0) as u64))
				.collect();
			Ok(VerifiedJoin {
				display_name: ok.display_name,
				account,
				skills: SkillSet::from_wire(&pairs),
			})
		}
		401 | 404 => Err(VerifyError::Denied),
		s => Err(VerifyError::Unavailable(format!("http {s}"))),
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountLookupBody {
	account_id: String,
}

/// Resolve a display name to its account id. Used exactly once per admin at
/// boot — the SERVER never asks this question again, because the answer it
/// keeps is the id.
pub fn lookup_account(url: &str, name: &str) -> Result<AccountId, String> {
	let mut res = agent(Duration::from_secs(10))
		.get(format!("{url}/api/accounts/by-name/{name}"))
		.call()
		.map_err(|e| e.to_string())?;
	match res.status().as_u16() {
		200 => {
			let body: AccountLookupBody = res
				.body_mut()
				.read_json()
				.map_err(|e| format!("bad lookup response: {e}"))?;
			AccountId::parse(&body.account_id)
				.ok_or_else(|| format!("master sent a bad account id: {}", body.account_id))
		}
		404 => Err(format!("no account is named {name:?}")),
		s => Err(format!("http {s}")),
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

/// What a skill report came back as.
#[derive(Debug, PartialEq, Eq)]
pub enum ReportOutcome {
	Accepted,
	/// 404: the master lost our registration, exactly as a heartbeat's.
	Lost,
	/// 403: this address is not on the master's trusted list. Final for
	/// this registration — the master decided, and re-asking is spam.
	Refused,
}

/// Post one batch of XP totals to the board.
pub fn report_skills(
	url: &str,
	reg: &Registration,
	rows: &[(AccountId, SkillSet)],
) -> Result<ReportOutcome, String> {
	let body = SkillReportBody {
		players: rows
			.iter()
			.map(|(account, skills)| SkillReportRow {
				account_id: account.to_string(),
				skills: skills
					.to_wire()
					.into_iter()
					.map(|(skill, xp)| SkillXpBody {
						skill,
						xp: xp.min(i64::MAX as u64) as i64,
					})
					.collect(),
			})
			.collect(),
	};
	let res = agent(Duration::from_secs(10))
		.post(format!("{url}/api/servers/{}/skills", reg.server_id))
		.header("authorization", &format!("Bearer {}", reg.server_key))
		.send_json(&body)
		.map_err(|e| e.to_string())?;
	match res.status().as_u16() {
		204 => Ok(ReportOutcome::Accepted),
		404 => Ok(ReportOutcome::Lost),
		403 => Ok(ReportOutcome::Refused),
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

/// Register (retrying forever with capped backoff — joins are refused until it lands), then
/// heartbeat on `HEARTBEAT_INTERVAL`; a 404 means the master restarted, so drop the registration
/// and re-register. `cert_hash` is shared with the cert-rotation task, which updates it and
/// signals `reregister` so the master hands out the new pin; the hash is re-read on every
/// registration attempt, so a rotation landing mid-retry still registers the current cert.
pub async fn run(
	state: Arc<MasterState>,
	mut cfg: MasterConfig,
	cert_hash: Arc<RwLock<[u8; 32]>>,
	reregister: Arc<tokio::sync::Notify>,
	players: Arc<AtomicUsize>,
	key_path: Option<std::path::PathBuf>,
	outbox: Arc<SkillOutbox>,
) {
	register_with_retry(&state, &mut cfg, &cert_hash, key_path.as_deref()).await;
	let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
	interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	interval.tick().await; // first tick fires immediately; skip it
	loop {
		tokio::select! {
			_ = reregister.notified() => {
				state.set_registration(None);
				register_with_retry(&state, &mut cfg, &cert_hash, key_path.as_deref()).await;
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
				register_with_retry(&state, &mut cfg, &cert_hash, key_path.as_deref()).await;
				continue;
			}
			Ok(Err(e)) => {
				log::warn!("heartbeat failed: {e}");
				continue;
			}
			Err(e) => {
				log::warn!("heartbeat task failed: {e}");
				continue;
			}
		}
		// The board, on the heartbeat's cadence and only behind a heartbeat
		// that landed: one batch a beat, so a burst of level-ups drains
		// over a few beats rather than in one oversized post.
		let Some(reg) = state.registration() else {
			continue;
		};
		if !reg.reports_skills || outbox.is_empty() {
			continue;
		}
		let rows = outbox.drain(REPORT_BATCH);
		let url = state.url.clone();
		let outbox = outbox.clone();
		let report = tokio::task::spawn_blocking(move || {
			let outcome = report_skills(&url, &reg, &rows);
			(outcome, rows)
		})
		.await;
		match report {
			Ok((Ok(ReportOutcome::Accepted), rows)) => {
				log::info!("reported {} player(s)' skills to the board", rows.len());
			}
			Ok((Ok(ReportOutcome::Refused), _)) => {
				log::warn!(
					"the master does not trust this server to report skills; dropping the report"
				);
				let mut reg = state.registration().expect("registration checked above");
				reg.reports_skills = false;
				state.set_registration(Some(reg));
			}
			Ok((Ok(ReportOutcome::Lost), rows)) => {
				outbox.restore(rows);
				state.set_registration(None);
			}
			Ok((Err(e), rows)) => {
				log::warn!("skill report failed: {e}");
				outbox.restore(rows);
			}
			Err(e) => log::warn!("skill report task failed: {e}"),
		}
	}
}

async fn register_with_retry(
	state: &MasterState,
	cfg: &mut MasterConfig,
	cert_hash: &RwLock<[u8; 32]>,
	key_path: Option<&std::path::Path>,
) {
	let mut delay = RETRY_MIN;
	loop {
		let cfg_c = cfg.clone();
		let hash = *cert_hash.read().unwrap();
		let attempt = tokio::task::spawn_blocking(move || register(&cfg_c, &hash)).await;
		match attempt {
			Ok(Ok(reg)) => {
				log::info!(
					"registered with master {} as {} (listed at {}:{})",
					cfg.url,
					reg.server_id,
					reg.observed_host,
					cfg.advertise_port
				);
				if reg.reports_skills {
					log::info!("the master trusts this server: skill totals will reach the board");
				} else {
					log::info!("not on the master's trusted list: skill totals stay local");
				}
				// Hold on to the key: it is what makes the NEXT registration
				// a re-registration rather than a claim someone else could
				// have made. Usually unchanged; new on a first run, and new
				// again if the master had to re-key us.
				if cfg.server_key.as_deref() != Some(reg.server_key.as_str()) {
					cfg.server_key = Some(reg.server_key.clone());
					if let Some(path) = key_path {
						store_key(path, &reg.server_key);
					}
				}
				state.set_registration(Some(reg));
				return;
			}
			Ok(Err(RegisterError::Unreachable(detail))) => {
				let host_hint = match &cfg.advertise_host {
					Some(host) => {
						format!("--advertise-host {host} is the address the internet sees")
					}
					None => "this machine's public IP (auto-detected from the registration) \
						 accepts inbound traffic"
						.to_string(),
				};
				log::error!(
					"master says we are UNREACHABLE ({detail}) — clients cannot join.\n\
					 Check that {host_hint}\n\
					 and that udp {} is forwarded to this machine. Retrying in {}s.",
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

/// The file the registration key lives in, inside the server's data
/// directory. Named so an operator backing up `./data` takes it with them —
/// losing it is recoverable (the master re-keys a claim nothing is live on)
/// but it means a window where the address is unheld.
pub const KEY_FILE: &str = "master-key";

/// Read a stored registration key. A missing file is the normal first-run
/// case, not an error.
pub fn load_key(path: &std::path::Path) -> Option<String> {
	let raw = std::fs::read_to_string(path).ok()?;
	let key = raw.trim().to_string();
	(!key.is_empty()).then_some(key)
}

/// Persist the registration key, owner-readable only. It is a bearer credential for one
/// `host:port` on one master: whoever holds it can re-register that address, which means renaming
/// the server or pointing it at a different machine. Not catastrophic — it grants no access to
/// player data and cannot mint join tokens — but there is no reason for it to be world-readable
/// beside a world journal that is.
fn store_key(path: &std::path::Path, key: &str) {
	let write = || -> std::io::Result<()> {
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)?;
		}
		std::fs::write(path, key)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		Ok(())
	};
	match write() {
		Ok(()) => log::info!("stored master registration key in {}", path.display()),
		// Not fatal: the server runs fine, it just has to re-claim its
		// address on every restart, and will fail to while anything else is
		// live there.
		Err(e) => log::warn!(
			"could not persist the master registration key to {} ({e}) — \
			 this server will re-claim its address on each restart",
			path.display()
		),
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
