//! The web screenshot: snapshot the WebGPU canvas (its image is the last
//! presented frame) and hand it to the browser as a PNG download. Runs
//! inside the keypress's own task — `after_game_input` — the same
//! gesture rule the clipboard and fullscreen calls live under.
//!
//! `toDataURL`, deliberately not the async `toBlob`: the click has to
//! land while the keypress's user activation is still on the stack, or
//! Chrome treats it as a gesture-less download and raises the Save As
//! dialog (or blocks it outright) instead of just saving the file. The
//! synchronous encode hitches the frame by the PNG's worth of work —
//! the native path's blocking map wait makes the same trade.

use std::sync::atomic::{AtomicU32, Ordering};
use wasm_bindgen::JsCast;

/// Session-local shot counter, so the files read digblock-1.png,
/// digblock-2.png, ... in the order they were taken; the browser
/// uniquifies against earlier sessions' files on its own.
static SHOT_N: AtomicU32 = AtomicU32::new(0);

/// Encode the canvas as a PNG data URL and click a temporary anchor at
/// it, all inside the caller's task. Every failure degrades to a log
/// line; a screenshot must never take the game down.
pub(crate) fn download_canvas_png(canvas: &web_sys::HtmlCanvasElement) {
	let Ok(url) = canvas.to_data_url() else {
		log::warn!("screenshot: canvas snapshot refused");
		return;
	};
	let n = SHOT_N.fetch_add(1, Ordering::Relaxed) + 1;
	if let Some(doc) = web_sys::window().and_then(|w| w.document())
		&& let Ok(a) = doc.create_element("a")
		&& let Ok(a) = a.dyn_into::<web_sys::HtmlAnchorElement>()
	{
		a.set_href(&url);
		a.set_download(&format!("digblock-{n}.png"));
		a.click();
	}
}
