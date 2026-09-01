//! The shape editor's IO seam: the engine does no file IO, so the launcher
//! injects a command/response channel pair (the `MenuServices` pattern).
//! Desktop serves it from a directory of `.shape` files; web serves Save as
//! a browser download and PickAndLoad through the file picker. Absent
//! services = built-ins only, save disabled with a status message.

/// Commands the editor sends; every one eventually answers with a
/// [`ShapeIoResp`] (a cancelled web picker being the tolerated exception).
pub enum ShapeIoCmd {
	/// List the store's shape names.
	List,
	/// Read one shape's text by name.
	Load { name: String },
	/// Write one shape's text under a name.
	Save { name: String, text: String },
	/// Open the platform file picker and read the chosen file (web).
	PickAndLoad,
}

pub enum ShapeIoResp {
	Listing(Result<Vec<String>, String>),
	Loaded {
		name: String,
		result: Result<String, String>,
	},
	Saved {
		name: String,
		result: Result<(), String>,
	},
}

/// The injected channel pair; the launcher owns the other ends.
pub struct ShapeServices {
	pub cmd_tx: std::sync::mpsc::Sender<ShapeIoCmd>,
	pub resp_rx: std::sync::mpsc::Receiver<ShapeIoResp>,
}
