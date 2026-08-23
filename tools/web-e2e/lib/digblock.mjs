// Drive the web build from node the way `just run` drives the desktop
// one. One page contract on the far side (index.html + digblock-platform):
// `digblock-gpu-ready` → `digblock-first-frame` → (`digblock-exit` when a
// `?frames=` run ends); `digblock-fatal` when startup cannot continue. A
// listener for each is installed BEFORE the page's own script runs, so no
// event can be missed, and every console line is kept so a failure can
// quote the engine rather than guess.
import fs from "node:fs";
import path from "node:path";
import { chromium } from "@playwright/test";
import { decode, stats, diff, detail } from "./png.mjs";

/** Chrome, with WebGPU on. Headless Chrome ships WebGPU behind the same
 *  flag shadercheck uses; the blocklist override is for the CI-class GPUs
 *  (and the software fallback) Chrome would otherwise refuse. */
export const CHROME_ARGS = [
	"--enable-unsafe-webgpu",
	"--ignore-gpu-blocklist",
	"--enable-features=Vulkan,SkiaGraphite",
	"--autoplay-policy=no-user-gesture-required",
];

/** A bare `{name: value}` → query string: `true` is a flag, `null`/`undefined` is absent. */
export function query(params = {}) {
	const parts = [];
	for (const [k, v] of Object.entries(params)) {
		if (v === undefined || v === null || v === false) continue;
		parts.push(v === true ? k : `${k}=${encodeURIComponent(String(v))}`);
	}
	return parts.length ? `?${parts.join("&")}` : "";
}

/** The rule a caller would otherwise have to know: a run that ends itself
 *  is a capture unless it was asked to be a played or joined one —
 *  `frames` alone boots to the menu and photographs its dim over whatever
 *  scene was asked for. */
export function normalize(params) {
	if (params.frames != null && !params.play && !params.join && params.capture == null) {
		return { ...params, capture: true };
	}
	return params;
}

/** Launch the browser. `channel: chrome` is the installed Google Chrome —
 *  the browser players run, and the one `check-shaders` already needs —
 *  so nothing downloads; `DIG_CHROME` points elsewhere, as for shadercheck. */
export async function launch({ headed = false, slowMo = 0 } = {}) {
	const opts = { headless: !headed, slowMo, args: CHROME_ARGS };
	if (process.env.DIG_CHROME) opts.executablePath = process.env.DIG_CHROME;
	else opts.channel = "chrome";
	return chromium.launch(opts);
}

const INIT = () => {
	const dig = (window.__dig = { events: [], fatal: null, firstFrame: false, exited: false, gpuReady: false });
	for (const name of ["digblock-gpu-ready", "digblock-first-frame", "digblock-exit", "digblock-fatal"]) {
		addEventListener(name, (e) => {
			dig.events.push({ name, t: performance.now(), detail: e.detail ?? null });
			if (name === "digblock-gpu-ready") dig.gpuReady = true;
			if (name === "digblock-first-frame") dig.firstFrame = true;
			if (name === "digblock-exit") dig.exited = true;
			if (name === "digblock-fatal") dig.fatal = String(e.detail ?? name);
		});
	}
};

// Chromium's DPR emulation is only skin deep: `devicePixelRatio` reports
// the emulated ratio, but a ResizeObserver's `devicePixelContentBoxSize`
// keeps answering in the HOST display's device pixels — and that is the
// box winit sizes the canvas from, so the two disagree by the ratio
// between them in whichever direction they differ: an emulated 2× page on
// a 1× host renders into a 1× backing and stretches it, and an emulated 1×
// page on a Retina host gets a backing twice what its scale factor says.
// Either way the UI is drawn for the wrong surface. A window with no
// emulation over it does not do this (`--native --headed` shows
// 2400×1616 at 2×). Derive the device box from the CSS box and the ratio
// the page believes, which is what a real display would have reported.
const HONEST_DPR = () => {
	const proto = ResizeObserverEntry.prototype;
	const desc = Object.getOwnPropertyDescriptor(proto, "devicePixelContentBoxSize");
	if (!desc?.get) return;
	Object.defineProperty(proto, "devicePixelContentBoxSize", {
		configurable: true,
		get() {
			const css = this.contentBoxSize?.[0];
			if (!css) return desc.get.call(this);
			const r = devicePixelRatio;
			return [{ inlineSize: Math.round(css.inlineSize * r), blockSize: Math.round(css.blockSize * r) }];
		},
	});
};

export class Dig {
	/** @param {import('@playwright/test').Page} page */
	constructor(page, url) {
		this.page = page;
		this.url = url;
		this.console = [];
		this.errors = [];
		page.on("console", (m) => this.console.push({ type: m.type(), text: m.text() }));
		page.on("pageerror", (e) => this.errors.push(String(e)));
		page.on("crash", () => this.errors.push("page crashed"));
	}

	/** The canvas as the browser sees it: CSS box, backing store, ratio. */
	canvas() {
		return this.page.evaluate(() => {
			const c = document.getElementById("digblock-canvas");
			return { dpr: devicePixelRatio, css: [c.clientWidth, c.clientHeight], backing: [c.width, c.height] };
		});
	}

	/** The engine's view of the run, straight from the page. */
	state() {
		return this.page.evaluate(() => window.__dig);
	}

	/** What the page can still say about a stall. Every startup stage can
	 *  HANG rather than fail, and both ends already know which one: the
	 *  lifecycle events say how far the platform got, and the loading
	 *  overlay's own watchdog writes the stalled stage (and the navigation
	 *  type) into `#status`. Without them a wasm fetch that never
	 *  returned, an adapter that never resolved and a first frame that
	 *  never presented all read as the same generic timeout. */
	async #diagnose() {
		return this.page
			.evaluate(() => {
				const d = globalThis.__dig ?? {};
				const events = (d.events ?? []).map((e) => `${e.name}@${Math.round(e.t)}ms`);
				// The overlay removes `#status` once the first frame lands,
				// so text here is the page still loading — including the
				// watchdog's "Still …" once a stage has stopped moving.
				return { gpuReady: !!d.gpuReady, events, status: document.getElementById("status")?.textContent ?? null };
			})
			.catch(() => null);
	}

	async #wait(pred, what, timeout) {
		const t0 = Date.now();
		for (;;) {
			const s = await this.state().catch(() => null);
			if (s?.fatal) throw this.failure(`the page reported a fatal error: ${s.fatal}`, await this.#diagnose());
			if (this.errors.length) throw this.failure(`the page threw: ${this.errors.join("; ")}`, await this.#diagnose());
			if (s && pred(s)) return s;
			if (Date.now() - t0 > timeout) {
				throw this.failure(`timed out after ${timeout} ms waiting for ${what}`, await this.#diagnose());
			}
			await this.page.waitForTimeout(50);
		}
	}

	/** Resolves once the first frame is on the canvas. */
	firstFrame(timeout = 60_000) {
		return this.#wait((s) => s.firstFrame, "the first frame", timeout);
	}

	/** Resolves once a `?frames=N` run has ended. */
	exit(timeout = 120_000) {
		return this.#wait((s) => s.exited, "digblock-exit (is ?frames= set?)", timeout);
	}

	/** An Error carrying the stalled stage and the console's tail, so a
	 *  failure names where the boot stopped and quotes the engine. */
	failure(msg, diag = null) {
		const lines = [msg];
		if (diag) {
			const reached = diag.events.length ? diag.events.join(" → ") : "no lifecycle event";
			lines.push(`  reached: ${reached}${diag.gpuReady ? "" : " (the GPU never reported ready)"}`);
			if (diag.status) lines.push(`  the loading overlay says: ${JSON.stringify(diag.status)}`);
		}
		lines.push(`  url: ${this.url}`);
		const tail = this.console.slice(-15).map((l) => `  [${l.type}] ${l.text}`).join("\n");
		return new Error(`${lines.join("\n")}\n${tail || "  (no console output)"}`);
	}

	/** Every console line of the engine's periodic metrics report, parsed:
	 *  `{fps, avg, p95, p99, worst, ...}` — the same line `just run` logs. */
	metrics() {
		const out = [];
		for (const { text } of this.console) {
			const m = text.match(/([\d.]+) fps \| avg\s+([\d.]+) p95\s+([\d.]+) p99\s+([\d.]+) worst\s+([\d.]+) ms/);
			if (m) out.push({ fps: +m[1], avg: +m[2], p95: +m[3], p99: +m[4], worst: +m[5], line: text });
		}
		return out;
	}

	/** Console lines that look like trouble — panics, errors, wgpu validation. */
	trouble() {
		return this.console.filter((l) => l.type === "error" || /panicked|RuntimeError|validation error/i.test(l.text));
	}

	/** Press keys in order: `"k"`, `"Escape"`, `"Shift+k"` — Playwright's spellings. */
	async keys(...names) {
		for (const n of names.flatMap((s) => s.split(",")).map((s) => s.trim()).filter(Boolean)) {
			await this.page.keyboard.press(n);
			await this.page.waitForTimeout(60);
		}
	}

	/** Photograph the canvas. Returns the decoded PNG; writes it when given a path. */
	async shot(file) {
		const buf = await this.page.screenshot({ type: "png" });
		if (file) {
			fs.mkdirSync(path.dirname(path.resolve(file)), { recursive: true });
			fs.writeFileSync(file, buf);
		}
		return decode(buf);
	}
}

/**
 * Open the game at `origin + query` in a fresh context and wait for its
 * first frame. `size` is the canvas in CSS px and `dpr` the backing scale —
 * the web's `--size` and the display it lands on, which the desktop can
 * only choose with `--display`.
 */
export async function boot(browser, origin, params = {}, { size = [1280, 800], dpr = 1, mobile = false, native = false, timeout = 60_000 } = {}) {
	// `native` drops Playwright's viewport/DPR emulation: the page gets the
	// browser's own window and whatever devicePixelRatio it really has —
	// the only way to tell an emulation artifact from a bug in how the page
	// sizes itself. Headless that window is Chrome's default one at 1×, so
	// a question about a REAL display's ratio wants `--headed` as well.
	const context = await browser.newContext(native ? { viewport: null } : {
		viewport: { width: size[0], height: size[1] },
		deviceScaleFactor: dpr,
		hasTouch: mobile,
		isMobile: mobile,
	});
	try {
		const page = await context.newPage();
		await page.addInitScript(INIT);
		// Emulation on, at any ratio: the correction keys on the emulation,
		// not on the ratio asked for. `--dpr 1` on a Retina host is the
		// same bug mirrored — the page believes 1× while the device box
		// still answers in the display's real pixels — and where the two
		// already agree the correction is the identity.
		if (!native) await page.addInitScript(HONEST_DPR);
		const url = origin + "/" + (typeof params === "string" ? params : query(normalize(params)));
		const dig = new Dig(page, url);
		await page.goto(url);
		await dig.firstFrame(timeout);
		return dig;
	} catch (e) {
		// The suite runs ONE worker because the browser holds the GPU
		// (playwright.config.mjs), and the fixture can only track a context
		// this function RETURNED — so a boot that throws with its page left
		// open leaves it rendering for the rest of the run, and one real
		// timeout cascades into flakes that look unrelated to it.
		await context.close().catch(() => {});
		throw e;
	}
}

export { decode, stats, diff, detail };
