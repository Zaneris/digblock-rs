//! Compiles every shader in `digblock_assets::shader::ALL` with the WGSL front end that actually
//! ships, by handing them to a headless Chrome.
//!
//! **Why this is not the naga test.** `shaders_parse_and_validate` in digblock-assets is naga, the
//! PERMISSIVE end; Chrome uses Tint, which rejects things naga accepts, and the failure mode is
//! the worst available — a whole invalidated pipeline, in the browser only, with every native
//! gate green.
//!
//! **Why a browser rather than a `tint` binary.** Dawn publishes no standalone tint executable,
//! and a pinned one could drift from the Chrome a player is running.
//!
//! **What it does NOT cover.** Shader modules only; a module that compiles can still fail at
//! pipeline creation, where bindings are matched against layouts.
//! `every_pipeline_builds_without_a_gpu` in digblock-client is the other half and runs on naga —
//! neither subsumes the other.

use std::path::PathBuf;
use std::process::Command;

/// Both halves of the protocol between the page and this process. The page
/// prints one line per shader; anything without the marker is Chrome's own
/// chatter and is ignored.
const MARK: &str = "@@SC@@";

/// A WGSL body as a JavaScript string literal. `<` is escaped unconditionally rather than only
/// where it could close the script block: WGSL is full of `<` (generics, comparisons, shifts),
/// and deciding which ones are safe is a parsing problem this does not need to have.
fn js_string(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + s.len() / 8);
	out.push('"');
	for ch in s.chars() {
		match ch {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'<' => out.push_str("\\u003C"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

fn harness() -> String {
	let mut shaders = String::new();
	for (name, src) in digblock_assets::shader::ALL {
		shaders.push_str(&format!(
			"{{n:{},c:{}}},\n",
			js_string(name),
			js_string(src)
		));
	}
	format!(
		r#"<!doctype html><meta charset="utf-8"><body><script type="module">
const SHADERS = [
{shaders}];
// Newlines are folded because the transport is one console line per
// shader, and Tint's messages carry a source excerpt.
const say = m => console.log({MARK:?} + " " + String(m).replace(/\n/g, "\\n"));
try {{
	if (!navigator.gpu) say("FATAL navigator.gpu is missing — no WebGPU in this build");
	else {{
		const adapter = await navigator.gpu.requestAdapter();
		if (!adapter) say("FATAL requestAdapter returned null — no usable GPU");
		else {{
			const device = await adapter.requestDevice();
			// An invalid module also fires an uncaptured device error;
			// swallow it so the report is ours alone.
			device.addEventListener("uncapturederror", e => e.preventDefault());
			for (const s of SHADERS) {{
				let errs = [];
				try {{
					const m = device.createShaderModule({{ code: s.c }});
					const info = await m.getCompilationInfo();
					errs = [...info.messages]
						.filter(x => x.type === "error")
						.map(x => x.lineNum + ":" + x.linePos + " " + x.message);
				}} catch (e) {{ errs = ["threw: " + e]; }}
				say(errs.length ? "ERR " + s.n + " :: " + errs.join(" ;; ") : "OK " + s.n);
			}}
		}}
	}}
}} catch (e) {{ say("FATAL " + e); }}
say("END");
</script></body>
"#
	)
}

/// Chrome, in the places it lives. `DIG_CHROME` wins so an unusual install
/// — or Chromium, or a channel other than stable — needs no code change.
fn find_chrome() -> Option<PathBuf> {
	if let Ok(p) = std::env::var("DIG_CHROME") {
		let p = PathBuf::from(p);
		return p.exists().then_some(p);
	}
	let mac = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
	if mac.exists() {
		return Some(mac);
	}
	let path = std::env::var("PATH").unwrap_or_default();
	for name in [
		"google-chrome",
		"google-chrome-stable",
		"chromium",
		"chromium-browser",
	] {
		for dir in std::env::split_paths(&path) {
			let cand = dir.join(name);
			if cand.exists() {
				return Some(cand);
			}
		}
	}
	None
}

fn main() {
	let Some(chrome) = find_chrome() else {
		eprintln!(
			"shadercheck: no Chrome found.\n\
			 \n\
			 This gate compiles the shaders with the front end that actually\n\
			 ships them, so it needs the browser — naga cannot answer the\n\
			 question (see this crate's module docs).\n\
			 \n\
			   macOS: install Google Chrome\n\
			   linux: apt install google-chrome-stable  (or chromium)\n\
			   any:   DIG_CHROME=/path/to/chrome just check-shaders"
		);
		std::process::exit(1);
	};

	// A dedicated profile: without one, a headless run can collide with
	// the Chrome the developer has open, and the failure looks like a
	// shader problem rather than a locked profile directory.
	let dir = std::env::temp_dir().join("digblock-shadercheck");
	let _ = std::fs::create_dir_all(&dir);
	let page = dir.join("harness.html");
	if let Err(e) = std::fs::write(&page, harness()) {
		eprintln!("shadercheck: could not write {}: {e}", page.display());
		std::process::exit(1);
	}

	// Chrome is READ, not awaited. A headless run that has finished its work does not necessarily
	// exit — `--dump-dom` wants a virtual-time budget that real GPU waits can stall, and
	// `window.close()` leaves the process up. The harness announces its own end instead and the
	// browser is killed the moment it does, which also makes the deadline below a real bound.
	let mut child = match Command::new(&chrome)
		.args([
			"--headless=new",
			"--enable-unsafe-webgpu",
			"--enable-logging=stderr",
		])
		.arg(format!("--user-data-dir={}", dir.join("profile").display()))
		.arg(format!("file://{}", page.display()))
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::piped())
		.spawn()
	{
		Ok(c) => c,
		Err(e) => {
			eprintln!("shadercheck: could not run {}: {e}", chrome.display());
			std::process::exit(1);
		}
	};

	let stderr = child.stderr.take().expect("stderr was piped");
	let (tx, rx) = std::sync::mpsc::channel();
	std::thread::spawn(move || {
		for line in std::io::BufRead::lines(std::io::BufReader::new(stderr)) {
			let Ok(line) = line else { break };
			if tx.send(line).is_err() {
				break;
			}
		}
	});

	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
	let mut lines = Vec::new();
	let mut chatter = Vec::new();
	let mut finished = false;
	while !finished {
		let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
			break;
		};
		let Ok(line) = rx.recv_timeout(left) else {
			break; // the deadline, or Chrome closed the pipe
		};
		let Some(at) = line.find(MARK) else {
			chatter.push(line);
			continue;
		};
		// Chrome brackets a console message in quotes and appends its
		// origin; the payload is what sits between our marker and that.
		let rest = &line[at + MARK.len()..];
		let rest = rest.strip_prefix(' ').unwrap_or(rest);
		let payload = match rest.rfind("\", source: ") {
			Some(end) => &rest[..end],
			None => rest.trim_end_matches('"'),
		};
		finished = payload == "END";
		lines.push(payload.to_string());
	}
	let _ = child.kill();
	let _ = child.wait();

	if !finished {
		eprintln!("shadercheck: the harness never finished. Chrome said:\n");
		for line in chatter
			.iter()
			.rev()
			.take(20)
			.collect::<Vec<_>>()
			.iter()
			.rev()
		{
			eprintln!("  {line}");
		}
		std::process::exit(1);
	}

	let mut failed = 0usize;
	let mut checked = 0usize;
	for line in &lines {
		if let Some(msg) = line.strip_prefix("FATAL ") {
			eprintln!("shadercheck: {msg}");
			std::process::exit(1);
		} else if let Some(rest) = line.strip_prefix("ERR ") {
			checked += 1;
			failed += 1;
			let (name, msg) = rest.split_once(" :: ").unwrap_or((rest, ""));
			eprintln!("{name}:");
			for one in msg.split(" ;; ") {
				eprintln!("  {}", one.replace("\\n", "\n  "));
			}
		} else if line.starts_with("OK ") {
			checked += 1;
		}
	}

	// A silent pass over zero shaders is the way this gate turns itself
	// off, so the count is part of the result rather than a nicety.
	if checked != digblock_assets::shader::ALL.len() {
		eprintln!(
			"shadercheck: reported on {checked} shaders, expected {}",
			digblock_assets::shader::ALL.len()
		);
		std::process::exit(1);
	}
	if failed > 0 {
		eprintln!(
			"\nshadercheck: {failed}/{checked} shaders rejected by Chrome's WGSL front end.\n\
			 Line numbers are in the COMPOSED source — the shared preludes are\n\
			 prepended, so subtract their length to land in the body file."
		);
		std::process::exit(1);
	}
	println!("shadercheck: {checked} shaders compile in Chrome");
}
