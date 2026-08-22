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
import { decode, stats, diff } from "./png.mjs";

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

	/** The engine's view of the run, straight from the page. */
	state() {
		return this.page.evaluate(() => window.__dig);
	}

	async #wait(pred, what, timeout) {
		const t0 = Date.now();
		for (;;) {
			const s = await this.state().catch(() => null);
			if (s?.fatal) throw this.failure(`the page reported a fatal error: ${s.fatal}`);
			if (this.errors.length) throw this.failure(`the page threw: ${this.errors.join("; ")}`);
			if (s && pred(s)) return s;
			if (Date.now() - t0 > timeout) throw this.failure(`timed out after ${timeout} ms waiting for ${what}`);
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

	/** An Error carrying the console's tail, so a failure quotes the engine. */
	failure(msg) {
		const tail = this.console.slice(-15).map((l) => `  [${l.type}] ${l.text}`).join("\n");
		return new Error(`${msg}\n  url: ${this.url}\n${tail || "  (no console output)"}`);
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
export async function boot(browser, origin, params = {}, { size = [1280, 800], dpr = 1, mobile = false, timeout = 60_000 } = {}) {
	const context = await browser.newContext({
		viewport: { width: size[0], height: size[1] },
		deviceScaleFactor: dpr,
		hasTouch: mobile,
		isMobile: mobile,
	});
	const page = await context.newPage();
	await page.addInitScript(INIT);
	const url = origin + "/" + (typeof params === "string" ? params : query(normalize(params)));
	const dig = new Dig(page, url);
	await page.goto(url);
	await dig.firstFrame(timeout);
	return dig;
}

export { decode, stats, diff };
