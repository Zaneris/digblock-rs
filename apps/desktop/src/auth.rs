//! Desktop sign-in: the RFC 8252 loopback flow, and the session it produces.
//!
//! The game opens the system browser at the master's sign-in page with a
//! redirect back to a loopback port we are listening on. The browser does
//! whichever provider the player picked; the master bounces back to
//! `http://127.0.0.1:<port>/cb?code=…&state=…`; we redeem that code over our own
//! HTTPS connection for a session token. The client never handles a provider
//! token, and the master never handles our verifier.
//!
//! Two secrets guard the hop the browser makes, because a URL is the least
//! private thing on a machine — history, logs, sometimes a clipboard. `state`
//! proves the callback belongs to the request we started; the PKCE verifier
//! proves the redemption does (RFC 7636).

use digblock_net::pkce::{b64url, s256, urlencode};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long the browser half may take. Generous: it can include creating a
/// Google account from scratch, and the only cost of waiting is a menu
/// screen the player can cancel out of.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(180);

/// A signed-in session, as stored on this machine.
///
/// A credential FOR an identity, never the identity itself: it expires, the account screen can
/// revoke it, and it is presented only to the master. A game server never sees it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Session {
	pub account_id: String,
	/// `None` until the player picks one.
	#[serde(default)]
	pub display_name: Option<String>,
	pub token: String,
}

pub(crate) fn session_path() -> PathBuf {
	std::env::var("DIG_SESSION_FILE")
		.map(PathBuf::from)
		.unwrap_or_else(|_| {
			dirs::data_dir()
				.map(|d| d.join("digblock").join("session.json"))
				.unwrap_or_else(|| PathBuf::from("./digblock-session.json"))
		})
}

pub(crate) fn load(path: &Path) -> Option<Session> {
	let bytes = std::fs::read(path).ok()?;
	serde_json::from_slice(&bytes).ok()
}

/// Persist owner-readable only.
///
/// For as long as it lives, this token can join servers as this player, and nothing else on the
/// machine has any business reading it.
pub(crate) fn store(path: &Path, session: &Session) {
	let write = || -> std::io::Result<()> {
		if let Some(dir) = path.parent() {
			std::fs::create_dir_all(dir)?;
		}
		let bytes = serde_json::to_vec_pretty(session)?;
		std::fs::write(path, bytes)?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
		}
		Ok(())
	};
	if let Err(e) = write() {
		log::warn!(
			"could not save the session to {} ({e}) — you will sign in again next launch",
			path.display()
		);
	}
}

pub(crate) fn clear(path: &Path) {
	if let Err(e) = std::fs::remove_file(path)
		&& e.kind() != std::io::ErrorKind::NotFound
	{
		log::warn!("could not remove {}: {e}", path.display());
	}
}

/// Run the whole browser round trip. Blocking — the caller gives it a
/// thread of its own.
pub(crate) fn sign_in(master_url: &str, cancel: &Arc<AtomicBool>) -> Result<Session, String> {
	let verifier = random_b64(32);
	let challenge = s256(&verifier);
	let state = random_b64(16);

	// Port 0: the OS picks. RFC 8252 §7.3 requires the authorization server
	// to ignore the port when matching a loopback redirect, precisely
	// because a native client cannot reserve one in advance.
	let listener = TcpListener::bind("127.0.0.1:0")
		.map_err(|e| format!("could not open a local port: {e}"))?;
	let port = listener.local_addr().map_err(|e| e.to_string())?.port();
	listener.set_nonblocking(true).map_err(|e| e.to_string())?;

	let redirect = format!("http://127.0.0.1:{port}/cb");
	let url = format!(
		"{master_url}/auth/start?redirect_uri={}&state={}&code_challenge={}&platform=desktop",
		urlencode(&redirect),
		urlencode(&state),
		urlencode(&challenge),
	);
	if let Err(e) = open::that_detached(&url) {
		// Not fatal: the listener is up and the player can paste the URL.
		// Log it in full so they can.
		log::warn!("could not open a browser ({e}). Sign in here:\n  {url}");
	}

	let code = wait_for_callback(&listener, &state, cancel)?;
	let session = exchange(master_url, &code, &verifier)?;
	Ok(session)
}

/// Accept until one request carries our code, or we run out of time.
///
/// Deliberately a loop rather than "take the first connection": a browser
/// will happily ask for `/favicon.ico` first, and treating that as the
/// answer would fail every sign-in on some browsers and none on others.
fn wait_for_callback(
	listener: &TcpListener,
	state: &str,
	cancel: &Arc<AtomicBool>,
) -> Result<String, String> {
	let deadline = Instant::now() + SIGN_IN_TIMEOUT;
	while Instant::now() < deadline {
		if cancel.load(Ordering::Relaxed) {
			return Err("sign-in cancelled".into());
		}
		let (stream, _) = match listener.accept() {
			Ok(pair) => pair,
			Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
				std::thread::sleep(Duration::from_millis(100));
				continue;
			}
			Err(e) => return Err(format!("local listener failed: {e}")),
		};
		let _ = stream.set_nonblocking(false);
		let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
		let mut reader = BufReader::new(&stream);
		let mut line = String::new();
		if reader.read_line(&mut line).is_err() {
			continue;
		}
		// "GET /cb?code=…&state=… HTTP/1.1"
		let target = line
			.split_whitespace()
			.nth(1)
			.unwrap_or_default()
			.to_string();
		let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
		let code = query_param(query, "code");
		let got_state = query_param(query, "state");

		let (status, body) = match (&code, &got_state) {
			(Some(_), Some(s)) if s == state => (
				"200 OK",
				page(
					"You're signed in",
					"Head back to DigBlock — you can close this tab.",
				),
			),
			(Some(_), _) => (
				"400 Bad Request",
				page(
					"Sign-in failed",
					"That response did not match the request DigBlock started.",
				),
			),
			_ => ("404 Not Found", page("Not found", "Nothing to see here.")),
		};
		let mut stream = stream;
		let _ = write!(
			stream,
			"HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
			 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
			body.len()
		);
		let _ = stream.flush();

		match (code, got_state) {
			// A mismatched state is not merely a miss: something else
			// aimed a callback at our port. Refuse the whole sign-in
			// rather than waiting for a "better" one.
			(Some(_), Some(s)) if s != state => {
				return Err("sign-in response did not match the request".into());
			}
			(Some(code), Some(_)) => return Ok(code),
			_ => continue, // favicon and friends
		}
	}
	Err("sign-in timed out".into())
}

fn page(title: &str, body: &str) -> String {
	format!(
		"<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
		 <style>body{{margin:0;min-height:100vh;display:grid;place-items:center;\
		 background:#101010;color:#f0f0f0;font:16px/1.5 system-ui,sans-serif}}\
		 main{{text-align:center;padding:2rem}}h1{{font-size:1.35rem;margin:0 0 .5rem}}\
		 p{{color:#c8c8c8;margin:0}}</style></head>\
		 <body><main><h1>{title}</h1><p>{body}</p></main></body></html>"
	)
}

// ---- master calls ---------------------------------------------------------

fn agent() -> ureq::Agent {
	ureq::Agent::new_with_config(
		ureq::Agent::config_builder()
			.timeout_global(Some(Duration::from_secs(10)))
			.http_status_as_error(false)
			.build(),
	)
}

/// Read `{ "error": "…" }` if the master sent one, else describe the status.
fn error_of(res: &mut ureq::http::Response<ureq::Body>) -> String {
	let status = res.status().as_u16();
	res.body_mut()
		.read_json::<serde_json::Value>()
		.ok()
		.and_then(|v| v["error"].as_str().map(str::to_string))
		.unwrap_or_else(|| match status {
			401 => "not signed in".into(),
			429 => "too many attempts — try again shortly".into(),
			_ => format!("http {status}"),
		})
}

fn session_of(res: &mut ureq::http::Response<ureq::Body>) -> Result<Session, String> {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		account_id: String,
		display_name: Option<String>,
		session_token: String,
	}
	let body: Body = res.body_mut().read_json().map_err(|e| e.to_string())?;
	Ok(Session {
		account_id: body.account_id,
		display_name: body.display_name,
		token: body.session_token,
	})
}

fn exchange(master_url: &str, code: &str, verifier: &str) -> Result<Session, String> {
	let mut res = agent()
		.post(format!("{master_url}/api/session/exchange"))
		.send_json(serde_json::json!({ "code": code, "verifier": verifier }))
		.map_err(|e| e.to_string())?;
	if res.status().as_u16() != 200 {
		return Err(error_of(&mut res));
	}
	session_of(&mut res)
}

/// Rotate a stored session at startup. A refusal means the token is gone
/// (expired, revoked, or replayed) and the caller signs out locally.
pub(crate) fn refresh(master_url: &str, token: &str) -> Result<Session, String> {
	let mut res = agent()
		.post(format!("{master_url}/api/session/refresh"))
		.header("authorization", &format!("Bearer {token}"))
		.send_json(serde_json::json!({ "platform": "desktop" }))
		.map_err(|e| e.to_string())?;
	if res.status().as_u16() != 200 {
		return Err(error_of(&mut res));
	}
	session_of(&mut res)
}

pub(crate) fn sign_out(master_url: &str, token: &str) {
	let _ = agent()
		.post(format!("{master_url}/api/session/signout"))
		.header("authorization", &format!("Bearer {token}"))
		.send_json(serde_json::json!({ "allDevices": false }));
}

/// Is this name free for me? The session goes along so that re-picking my
/// own name reads as available rather than as a collision with myself.
pub(crate) fn check_name(master_url: &str, token: &str, name: &str) -> (bool, Option<String>) {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		available: bool,
		reason: Option<String>,
	}
	let res = agent()
		.get(format!(
			"{master_url}/api/accounts/name-available?name={}",
			urlencode(name)
		))
		.header("authorization", &format!("Bearer {token}"))
		.call();
	match res {
		Ok(mut res) if res.status().as_u16() == 200 => match res.body_mut().read_json::<Body>() {
			Ok(b) => (b.available, b.reason),
			Err(e) => (false, Some(e.to_string())),
		},
		Ok(mut res) => (false, Some(error_of(&mut res))),
		Err(e) => (false, Some(e.to_string())),
	}
}

pub(crate) fn set_name(master_url: &str, token: &str, name: &str) -> Result<String, String> {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		display_name: String,
	}
	let mut res = agent()
		.post(format!("{master_url}/api/account/name"))
		.header("authorization", &format!("Bearer {token}"))
		.send_json(serde_json::json!({ "displayName": name }))
		.map_err(|e| e.to_string())?;
	if res.status().as_u16() != 200 {
		return Err(error_of(&mut res));
	}
	res.body_mut()
		.read_json::<Body>()
		.map(|b| b.display_name)
		.map_err(|e| e.to_string())
}

/// Development-only sign-in, for scripted runs (`--dev-signin`) that cannot
/// open a browser. The master honours it only in Development and refuses to
/// boot with it set anywhere else.
pub(crate) fn dev_sign_in(master_url: &str, subject: &str) -> Result<Session, String> {
	let mut res = agent()
		.post(format!("{master_url}/api/dev/signin"))
		.send_json(serde_json::json!({ "subject": subject, "platform": "desktop" }))
		.map_err(|e| e.to_string())?;
	if res.status().as_u16() != 200 {
		return Err(format!(
			"{} — is the master running with Master__Dev__AuthEnabled=true?",
			error_of(&mut res)
		));
	}
	session_of(&mut res)
}

// ---- small encodings ------------------------------------------------------

fn random_b64(bytes: usize) -> String {
	let mut buf = vec![0u8; bytes];
	for b in buf.iter_mut() {
		*b = rand::random();
	}
	b64url(&buf)
}

fn query_param(query: &str, key: &str) -> Option<String> {
	query.split('&').find_map(|pair| {
		let (k, v) = pair.split_once('=')?;
		(k == key).then(|| urldecode(v))
	})
}

fn urldecode(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
				Ok(b) => {
					out.push(b);
					i += 3;
				}
				Err(_) => {
					out.push(bytes[i]);
					i += 1;
				}
			},
			b'+' => {
				out.push(b' ');
				i += 1;
			}
			b => {
				out.push(b);
				i += 1;
			}
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The callback arrives as a raw request line, so the query parser is
	/// the only thing standing between a browser and a panic.
	#[test]
	fn query_parsing_survives_what_a_browser_actually_sends() {
		let q = "code=abc%2Ddef&state=xy%20z";
		assert_eq!(query_param(q, "code").as_deref(), Some("abc-def"));
		assert_eq!(query_param(q, "state").as_deref(), Some("xy z"));
		assert_eq!(query_param(q, "missing"), None);
		assert_eq!(query_param("", "code"), None);
		// Malformed percent escapes pass through rather than panicking.
		assert_eq!(query_param("code=%zz", "code").as_deref(), Some("%zz"));
		assert_eq!(query_param("code=%", "code").as_deref(), Some("%"));
	}

	#[test]
	fn urlencode_round_trips_through_urldecode() {
		for s in ["", "plain", "a b", "http://127.0.0.1:1234/cb", "+/=&?%"] {
			assert_eq!(urldecode(&urlencode(s)), s, "{s:?}");
		}
	}
}
