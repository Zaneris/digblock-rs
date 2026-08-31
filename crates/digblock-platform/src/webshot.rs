//! The web screenshot: snapshot the WebGPU canvas (its image is the last
//! presented frame) and hand it to the browser as a PNG download. Runs
//! inside the keypress's own task — `after_game_input` — the same
//! gesture rule the clipboard and fullscreen calls live under. The
//! callback uses `Closure::once_into_js`: wasm must never rely on Drop.

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

/// Ask the canvas for a PNG blob and click a temporary anchor at it.
/// Every failure degrades to a log line; a screenshot must never take
/// the game down.
pub(crate) fn download_canvas_png(canvas: &web_sys::HtmlCanvasElement, stamp: u64) {
	let cb = Closure::once_into_js(move |blob: wasm_bindgen::JsValue| {
		let Ok(blob) = blob.dyn_into::<web_sys::Blob>() else {
			log::warn!("screenshot: canvas produced no blob");
			return;
		};
		let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else {
			log::warn!("screenshot: blob URL refused");
			return;
		};
		if let Some(doc) = web_sys::window().and_then(|w| w.document())
			&& let Ok(a) = doc.create_element("a")
			&& let Ok(a) = a.dyn_into::<web_sys::HtmlAnchorElement>()
		{
			a.set_href(&url);
			a.set_download(&format!("digblock-{stamp}.png"));
			a.click();
		}
		let _ = web_sys::Url::revoke_object_url(&url);
	});
	if canvas.to_blob(cb.unchecked_ref()).is_err() {
		log::warn!("screenshot: canvas snapshot refused");
	}
}
