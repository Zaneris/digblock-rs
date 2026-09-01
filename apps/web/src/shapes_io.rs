//! The shape editor's IO on the web: Save hands the browser a download of
//! the `.shape` text, PickAndLoad opens the file picker and reads the
//! chosen file back. There is no persistent store here — List answers
//! empty, and the picker is how files come back in (both fire from a
//! keypress, so the transient-activation rules are satisfied). One
//! spawn_local task owns the channel, the `services.rs` pattern.

use digblock_client::{ShapeIoCmd, ShapeIoResp, ShapeServices};
use std::sync::mpsc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

pub(crate) fn start() -> ShapeServices {
	let (cmd_tx, cmd_rx) = mpsc::channel::<ShapeIoCmd>();
	let (resp_tx, resp_rx) = mpsc::channel::<ShapeIoResp>();
	wasm_bindgen_futures::spawn_local(run(cmd_rx, resp_tx));
	ShapeServices { cmd_tx, resp_rx }
}

async fn run(cmd_rx: mpsc::Receiver<ShapeIoCmd>, resp_tx: mpsc::Sender<ShapeIoResp>) {
	loop {
		while let Ok(cmd) = cmd_rx.try_recv() {
			let resp = match cmd {
				ShapeIoCmd::List => ShapeIoResp::Listing(Ok(Vec::new())),
				ShapeIoCmd::Load { name } => ShapeIoResp::Loaded {
					name,
					result: Err(String::from("no store on web: O imports a file")),
				},
				ShapeIoCmd::Save { name, text } => {
					let result = download(&name, &text);
					ShapeIoResp::Saved { name, result }
				}
				ShapeIoCmd::PickAndLoad => pick_and_load().await,
			};
			if resp_tx.send(resp).is_err() {
				return;
			}
		}
		crate::services::sleep_ms(150).await;
	}
}

fn document() -> Result<web_sys::Document, String> {
	web_sys::window()
		.and_then(|w| w.document())
		.ok_or_else(|| String::from("no document"))
}

/// Hand the text to the browser as a `<name>.shape` download.
fn download(name: &str, text: &str) -> Result<(), String> {
	let doc = document()?;
	let parts = js_sys::Array::of1(&JsValue::from_str(text));
	let blob = web_sys::Blob::new_with_str_sequence(&parts).map_err(|e| js_err(&e))?;
	let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|e| js_err(&e))?;
	let a: web_sys::HtmlAnchorElement = doc
		.create_element("a")
		.map_err(|e| js_err(&e))?
		.dyn_into()
		.map_err(|_| String::from("not an anchor"))?;
	a.set_href(&url);
	a.set_download(&format!("{name}.shape"));
	a.click();
	let _ = web_sys::Url::revoke_object_url(&url);
	Ok(())
}

/// Open the file picker and read the chosen `.shape` back. A dismissed
/// picker resolves through the input's `cancel` event.
async fn pick_and_load() -> ShapeIoResp {
	let fail = |m: String| ShapeIoResp::Loaded {
		name: String::new(),
		result: Err(m),
	};
	let doc = match document() {
		Ok(d) => d,
		Err(e) => return fail(e),
	};
	let input: web_sys::HtmlInputElement = match doc
		.create_element("input")
		.map_err(|e| js_err(&e))
		.and_then(|e| e.dyn_into().map_err(|_| String::from("not an input")))
	{
		Ok(i) => i,
		Err(e) => return fail(e),
	};
	input.set_type("file");
	input.set_accept(".shape");
	// Resolve on either outcome; the once-closures hand themselves to JS
	// (a leak per pick — a dev tool's, not a player's).
	let picked = {
		let input = input.clone();
		js_sys::Promise::new(&mut |resolve, _| {
			for event in ["change", "cancel"] {
				let r = resolve.clone();
				let cb = Closure::once_into_js(move |_: web_sys::Event| {
					let _ = r.call0(&JsValue::NULL);
				});
				let _ = input.add_event_listener_with_callback(event, cb.unchecked_ref());
			}
		})
	};
	input.click();
	let _ = JsFuture::from(picked).await;
	let Some(file) = input.files().and_then(|fs| fs.get(0)) else {
		return fail(String::from("no file chosen"));
	};
	let name = file.name();
	let stem = name.strip_suffix(".shape").unwrap_or(&name).to_string();
	match JsFuture::from(file.text()).await {
		Ok(v) => ShapeIoResp::Loaded {
			name: stem,
			result: Ok(v.as_string().unwrap_or_default()),
		},
		Err(e) => fail(js_err(&e)),
	}
}

fn js_err(v: &JsValue) -> String {
	v.as_string().unwrap_or_else(|| format!("{v:?}"))
}
