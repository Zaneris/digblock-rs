//! Browser sign-in, and the session it produces.
//!
//! Same master endpoints as desktop, different browser hop. A page cannot listen on a loopback
//! port, so the redirect target is the game page itself: navigate away to the master, come back
//! with the code and state in the query, exchange it, and scrub the URL.
//!
//! A top-level navigation rather than a popup, deliberately: a popup opened from the engine's
//! command drain is one task removed from the click that asked for it, which is exactly what a
//! popup blocker exists to stop.
//!
//! The PKCE verifier and the state outlive that reload in `sessionStorage` — per-tab, unreadable
//! from another origin, consumed on return. The page's own query rides along with them, because
//! the master refuses a redirect target that carries a query and losing `?master=` silently
//! repointed a dev-stack session at the production master. `take_callback` puts them back, minus
//! the one-shot actions (`?join`, `?devsignin`) that must not re-fire on return.

use digblock_net::pkce::{b64url, s256, urlencode};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

const SESSION_KEY: &str = "digblock.session";
const PENDING_KEY: &str = "digblock.signin";

/// The query keys a finished sign-in redirects back with. Named here
/// because [`take_callback`] scrubs them only once it has matched a
/// pending request, so the page's unknown-key check has to know them.
pub(crate) const CALLBACK_PARAMS: [&str; 2] = ["code", "state"];

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Session {
	pub account_id: String,
	#[serde(default)]
	pub display_name: Option<String>,
	pub token: String,
}

/// Verifier + state + the page's own query, parked across the navigation.
#[derive(serde::Serialize, serde::Deserialize)]
struct Pending {
	verifier: String,
	state: String,
	#[serde(default)]
	query: String,
}

fn local() -> Option<web_sys::Storage> {
	web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

fn session_storage() -> Option<web_sys::Storage> {
	web_sys::window().and_then(|w| w.session_storage().ok().flatten())
}

pub(crate) fn load() -> Option<Session> {
	let raw = local()?.get_item(SESSION_KEY).ok()??;
	serde_json::from_str(&raw).ok()
}

pub(crate) fn store(session: &Session) {
	if let (Some(s), Ok(json)) = (local(), serde_json::to_string(session)) {
		let _ = s.set_item(SESSION_KEY, &json);
	}
}

pub(crate) fn clear() {
	if let Some(s) = local() {
		let _ = s.remove_item(SESSION_KEY);
	}
}

/// Navigate to the master's sign-in page. Does not return in practice —
/// the document is replaced.
pub(crate) fn begin(master_url: &str) -> Result<(), String> {
	let verifier = random_b64(32)?;
	let state = random_b64(16)?;
	let challenge = s256(&verifier);

	let window = web_sys::window().ok_or("no window")?;
	let pending = serde_json::to_string(&Pending {
		verifier,
		state: state.clone(),
		query: surviving_query(&window),
	})
	.map_err(|e| e.to_string())?;
	session_storage()
		.ok_or("this browser is blocking storage, so sign-in cannot complete")?
		.set_item(PENDING_KEY, &pending)
		.map_err(|_| "could not stash the sign-in request".to_string())?;

	let here = window.location().href().map_err(|_| "no location")?;
	// The redirect target is this page stripped bare: the master refuses a
	// target with a query and appends its own. The query we were started
	// with is in `pending`, restored on return.
	let redirect = here.split(['?', '#']).next().unwrap_or(&here).to_string();
	let url = format!(
		"{master_url}/auth/start?redirect_uri={}&state={}&code_challenge={}&platform=web",
		urlencode(&redirect),
		urlencode(&state),
		urlencode(&challenge),
	);
	window
		.location()
		.set_href(&url)
		.map_err(|_| "could not open the sign-in page".to_string())
}

/// The `?code=&state=` a finished sign-in came back with, if this load is
/// one. Consumes the pending request and scrubs the URL either way — a
/// one-time code has no business sitting in the address bar or in history.
pub(crate) fn take_callback() -> Option<(String, String)> {
	let window = web_sys::window()?;
	let search = window.location().search().ok()?;
	let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
	let code = params.get("code")?;
	let state = params.get("state")?;

	let storage = session_storage()?;
	let pending: Pending = storage
		.get_item(PENDING_KEY)
		.ok()
		.flatten()
		.and_then(|raw| serde_json::from_str(&raw).ok())?;
	let _ = storage.remove_item(PENDING_KEY);
	scrub_url(&pending.query);

	// A state that does not match means this callback belongs to some
	// other request. Refuse it rather than redeeming it.
	if pending.state != state {
		log::warn!("sign-in response did not match the request that started it");
		return None;
	}
	Some((code, pending.verifier))
}

/// Drop the callback's `?code=&state=` without reloading, so a refresh or
/// a shared link does not carry the code — and put back the query that
/// configured this page, which the redirect could not carry.
fn scrub_url(restore: &str) {
	let Some(window) = web_sys::window() else {
		return;
	};
	let Ok(here) = window.location().href() else {
		return;
	};
	let mut clean = here.split(['?', '#']).next().unwrap_or(&here).to_string();
	if !restore.is_empty() {
		clean.push('?');
		clean.push_str(restore);
	}
	if let Ok(history) = window.history() {
		let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&clean));
	}
}

/// The page's current query, minus what must not fire twice: the one-shot
/// actions (`join` and its `server`, `devsignin`) and any stale callback
/// residue. What is left — `master`, the capture and settings params — is
/// configuration, and is exactly what sign-in must hand back to the page
/// it returns to.
fn surviving_query(window: &web_sys::Window) -> String {
	let Ok(search) = window.location().search() else {
		return String::new();
	};
	let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
		return String::new();
	};
	for one_shot in ["join", "server", "devsignin", "code", "state"] {
		params.delete(one_shot);
	}
	String::from(js_sys::Object::to_string(&params))
}

// ---- master calls ---------------------------------------------------------

async fn post_json(url: &str, bearer: Option<&str>, body: &str) -> Result<(u16, String), String> {
	let init = web_sys::RequestInit::new();
	init.set_method("POST");
	init.set_body(&JsValue::from_str(body));
	let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(js_err)?;
	request
		.headers()
		.set("content-type", "application/json")
		.map_err(js_err)?;
	if let Some(bearer) = bearer {
		request
			.headers()
			.set("authorization", &format!("Bearer {bearer}"))
			.map_err(js_err)?;
	}
	let window = web_sys::window().ok_or("no window")?;
	let resp = JsFuture::from(window.fetch_with_request(&request))
		.await
		.map_err(js_err)?;
	let resp: web_sys::Response = wasm_bindgen::JsCast::unchecked_into(resp);
	let text = JsFuture::from(resp.text().map_err(js_err)?)
		.await
		.map_err(js_err)?;
	Ok((resp.status(), text.as_string().unwrap_or_default()))
}

async fn get_text(url: &str, bearer: Option<&str>) -> Result<(u16, String), String> {
	let request = web_sys::Request::new_with_str(url).map_err(js_err)?;
	if let Some(bearer) = bearer {
		request
			.headers()
			.set("authorization", &format!("Bearer {bearer}"))
			.map_err(js_err)?;
	}
	let window = web_sys::window().ok_or("no window")?;
	let resp = JsFuture::from(window.fetch_with_request(&request))
		.await
		.map_err(js_err)?;
	let resp: web_sys::Response = wasm_bindgen::JsCast::unchecked_into(resp);
	let text = JsFuture::from(resp.text().map_err(js_err)?)
		.await
		.map_err(js_err)?;
	Ok((resp.status(), text.as_string().unwrap_or_default()))
}

fn js_err(v: JsValue) -> String {
	v.as_string()
		.unwrap_or_else(|| "javascript error".to_string())
}

/// `{ "error": "…" }` if the master sent one, else the status.
fn error_of(status: u16, body: &str) -> String {
	serde_json::from_str::<serde_json::Value>(body)
		.ok()
		.and_then(|v| v["error"].as_str().map(str::to_string))
		.unwrap_or_else(|| match status {
			401 => "not signed in".into(),
			429 => "too many attempts — try again shortly".into(),
			_ => format!("http {status}"),
		})
}

fn session_of(body: &str) -> Result<Session, String> {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		account_id: String,
		display_name: Option<String>,
		session_token: String,
	}
	let b: Body = serde_json::from_str(body).map_err(|e| e.to_string())?;
	Ok(Session {
		account_id: b.account_id,
		display_name: b.display_name,
		token: b.session_token,
	})
}

pub(crate) async fn exchange(
	master_url: &str,
	code: &str,
	verifier: &str,
) -> Result<Session, String> {
	let body = serde_json::json!({ "code": code, "verifier": verifier }).to_string();
	let (status, text) =
		post_json(&format!("{master_url}/api/session/exchange"), None, &body).await?;
	if status != 200 {
		return Err(error_of(status, &text));
	}
	session_of(&text)
}

pub(crate) async fn refresh(master_url: &str, token: &str) -> Result<Session, String> {
	let body = serde_json::json!({ "platform": "web" }).to_string();
	let (status, text) = post_json(
		&format!("{master_url}/api/session/refresh"),
		Some(token),
		&body,
	)
	.await?;
	if status != 200 {
		return Err(error_of(status, &text));
	}
	session_of(&text)
}

pub(crate) async fn sign_out(master_url: &str, token: &str) {
	let body = serde_json::json!({ "allDevices": false }).to_string();
	let _ = post_json(
		&format!("{master_url}/api/session/signout"),
		Some(token),
		&body,
	)
	.await;
}

pub(crate) async fn check_name(
	master_url: &str,
	token: &str,
	name: &str,
) -> (bool, Option<String>) {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		available: bool,
		reason: Option<String>,
	}
	let url = format!(
		"{master_url}/api/accounts/name-available?name={}",
		urlencode(name)
	);
	let bearer = (!token.is_empty()).then_some(token);
	match get_text(&url, bearer).await {
		Ok((200, text)) => match serde_json::from_str::<Body>(&text) {
			Ok(b) => (b.available, b.reason),
			Err(e) => (false, Some(e.to_string())),
		},
		Ok((status, text)) => (false, Some(error_of(status, &text))),
		Err(e) => (false, Some(e)),
	}
}

pub(crate) async fn set_name(master_url: &str, token: &str, name: &str) -> Result<String, String> {
	#[derive(serde::Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct Body {
		display_name: String,
	}
	let body = serde_json::json!({ "displayName": name }).to_string();
	let (status, text) = post_json(
		&format!("{master_url}/api/account/name"),
		Some(token),
		&body,
	)
	.await?;
	if status != 200 {
		return Err(error_of(status, &text));
	}
	serde_json::from_str::<Body>(&text)
		.map(|b| b.display_name)
		.map_err(|e| e.to_string())
}

/// Development-only sign-in, so `just play-web` and `?join` can run
/// without a browser round trip. Honoured only by a master in Development.
pub(crate) async fn dev_sign_in(master_url: &str, subject: &str) -> Result<Session, String> {
	let body = serde_json::json!({ "subject": subject, "platform": "web" }).to_string();
	let (status, text) = post_json(&format!("{master_url}/api/dev/signin"), None, &body).await?;
	if status != 200 {
		return Err(format!(
			"{} — is the master running with Master__Dev__AuthEnabled=true?",
			error_of(status, &text)
		));
	}
	session_of(&text)
}

// ---- small encodings ------------------------------------------------------
// Deliberate twins of the desktop launcher's. Sharing them would mean a
// crate for four small functions that must behave identically on two
// platforms with different randomness sources; the RFC test vector in each
// is what actually keeps them in step.

/// Fails rather than returning anything it had to invent. A weak PKCE verifier is worse than
/// useless — it looks like protection — so there is deliberately no fallback to `Math.random`,
/// and saying so while returning the untouched buffer is not a refusal either: an all-zero
/// verifier and state is guessable on the first try, so a leaked code would be redeemable by
/// anyone and the `state` check would pass for any callback. The only safe answer is no answer.
fn random_b64(bytes: usize) -> Result<String, String> {
	let mut buf = vec![0u8; bytes];
	web_sys::window()
		.and_then(|w| w.crypto().ok())
		.and_then(|c| c.get_random_values_with_u8_array(&mut buf).ok())
		.ok_or("this browser exposes no secure randomness, so sign-in cannot be made safe here")?;
	Ok(b64url(&buf))
}
