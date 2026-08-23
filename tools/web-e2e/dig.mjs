#!/usr/bin/env node
// `just run` for the browser. Every desktop flag that has a query twin is
// spelled the same way here and translated; anything else passes straight
// through as `?name[=value]`, and the PAGE rejects a key it does not know
// — the roster lives in Rust (apps/web/src/lib.rs, knobs::ALL), not here.
//
//   node tools/web-e2e/dig.mjs --seed 7 --screen alchemy --frames 60 --screenshot out.png
//   node tools/web-e2e/dig.mjs --vault --tod 90 --pos 10 70 10 --pitch -20 --frames 120 --screenshot vault.png
//   node tools/web-e2e/dig.mjs --play --seed 1 --keys k --settle 1500 --screenshot k.png --console
//   node tools/web-e2e/dig.mjs --headed --play --seed 1          (leave the window up)
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { spawnSync } from "node:child_process";
import { REPO, serve } from "./lib/serve.mjs";
import { boot, launch, normalize, query, stats } from "./lib/digblock.mjs";

// Desktop spelling → page spelling, for the flags whose names differ.
const RENAME = {
	"draw-dist": "draw",
	"fixed-dt": "fixeddt",
	"ui-scale": "uiscale",
	"max-lights": "maxlights",
	"render-scale": "renderscale",
	"gi-scale": "giscale",
	"master-url": "master",
	"dev-signin": "devsignin",
	"no-ssr": null, // handled below
};

// The scene roster, printed by the launcher that DEFINES it: the desktop
// reads `knobs::ALL` for its `--knobs` listing, so a knob added in Rust
// shows up here with no edit. The list used to be hand-copied into HELP
// and had already drifted by two names when a reviewer found it. Any
// built profile will do — the table is compiled in, not configured.
function knobs() {
	for (const profile of ["debug", "release", "profiling"]) {
		const bin = path.join(REPO, "target", profile, "digblock");
		if (!fs.existsSync(bin)) continue;
		const r = spawnSync(bin, ["--knobs"], { encoding: "utf8" });
		if (r.status === 0 && r.stdout.trim()) {
			return r.stdout.trimEnd().split("\n").map((l) => `  ${l}`).join("\n");
		}
	}
	return "  (none listed: the roster is digblock_client::knobs::ALL — `just run --knobs`\n"
		+ "   prints it, and this shows that listing once the desktop client is built)";
}

const HELP = () => `dig.mjs — drive the web build like \`just run\`

Harness flags (never reach the page):
  --screenshot FILE   photograph the canvas when the run ends (needs --frames) or after --settle
  --frames N          end the run at frame N (the web's --exit); implies --capture unless --play/--join
  --settle MS         wait this long after the first frame before acting (default 0; 1500 with --keys)
  --keys k,Escape     press keys after settling (Playwright names; comma-separated)
  --click X,Y         click the canvas at CSS px after the keys
  --size WxH          canvas size in CSS px (default 1280x800)
  --dpr N             backing scale (default 1; 2 is a Retina laptop)
  --mobile            touch device: coarse pointer + the mobile defaults (also sets ?mobile)
  --headed            show the window and leave it open until Ctrl-C
  --native            no viewport/DPR emulation: the browser's own window at its own
                      ratio (headless that is Chrome's default window at 1x — pair it
                      with --headed for a real display's)
  --console           echo every console line as it arrives
  --timeout MS        first-frame deadline (default 60000)
  --url ORIGIN        use a server already running instead of serving target/web
  --stats             print the screenshot's luminance / colour count

Page flags, desktop spelling accepted (translated to the query):
  --seed F  --tod DEG  --pos X Y Z  --yaw DEG  --pitch DEG  --fixed-dt S  --draw-dist N
  --workers N  --ui-scale F  --max-lights N  --render-scale F  --gi-scale N  --ssr/--no-ssr
  --overlay  --bench  --fly N  --warp N  --walk  --mute  --play  --capture
  --join --master-url URL --dev-signin SUBJECT  --nodatagrams
Every DIG_* scene and lever, as --name [value] (the desktop's own --knobs listing,
so this cannot drift from the table both launchers read):
${knobs()}
An unknown --name is passed through and the page refuses it (check the console).
`;

function parse(argv) {
	const h = { screenshot: null, frames: null, settle: null, keys: [], click: null, size: [1280, 800], dpr: 1,
		mobile: false, headed: false, native: false, console: false, timeout: 60_000, url: null, stats: false };
	const q = {};
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (!a.startsWith("--")) throw new Error(`unexpected argument ${a}`);
		// `--name=value` as well as `--name value`. Splitting here rather
		// than letting the whole token fall through to the passthrough arm
		// is what stops `--frames=90` reaching the page as a knob spelled
		// "frames=90": the page would accept `?frames=90` and the harness
		// would never learn the run ends, so it photographs frame one of an
		// interactive page and exits 0.
		const eq = a.indexOf("=");
		const name = eq === -1 ? a.slice(2) : a.slice(2, eq);
		let inline = eq === -1 ? null : a.slice(eq + 1);
		const next = () => {
			if (inline !== null) { const v = inline; inline = null; return v; }
			if (i + 1 >= argv.length || argv[i + 1].startsWith("--")) throw new Error(`--${name} wants a value`);
			return argv[++i];
		};
		const num = () => { const v = next(); const n = Number(v); if (!Number.isFinite(n)) throw new Error(`--${name} wants a number, got ${v}`); return n; };
		const peekValue = () => inline !== null || (i + 1 < argv.length && !argv[i + 1].startsWith("--"));
		switch (name) {
			case "help": case "h": console.log(HELP()); process.exit(0);
			case "screenshot": h.screenshot = next(); break;
			case "frames": h.frames = num(); break;
			case "settle": h.settle = num(); break;
			case "keys": h.keys.push(next()); break;
			case "click": { const v = next(); h.click = v.split(",").map(Number); if (h.click.length !== 2 || !h.click.every(Number.isFinite)) throw new Error("--click wants X,Y"); break; }
			case "size": { const m = next().match(/^(\d+)x(\d+)$/); if (!m) throw new Error("--size wants WxH"); h.size = [+m[1], +m[2]]; break; }
			case "dpr": h.dpr = num(); break;
			case "mobile": h.mobile = true; q.mobile = true; break;
			case "headed": h.headed = true; break;
			case "native": h.native = true; break;
			case "console": h.console = true; break;
			case "timeout": h.timeout = num(); break;
			case "url": h.url = next(); break;
			case "stats": h.stats = true; break;
			case "pos": q.pos = [next(), next(), next()].join(","); break;
			case "ssr": q.ssr = 1; break;
			case "no-ssr": q.ssr = 0; break;
			default: {
				const key = name in RENAME ? RENAME[name] : name;
				q[key] = peekValue() ? next() : true;
			}
		}
		if (inline !== null) throw new Error(`--${name} takes no value`);
	}
	if (h.frames != null) q.frames = h.frames;
	if (h.settle == null) h.settle = h.keys.length || h.click ? 1500 : 0;
	return { h, q };
}

async function main() {
	const { h, q } = parse(process.argv.slice(2));
	const server = h.url ? null : await serve();
	const origin = h.url ?? server.origin;
	const browser = await launch({ headed: h.headed });
	let code = 0;
	try {
		console.error(`→ ${origin}/${query(normalize(q))}`);
		const dig = await boot(browser, origin, q, { size: h.size, dpr: h.dpr, mobile: h.mobile, native: h.native, timeout: h.timeout });
		if (h.console) dig.page.on("console", (m) => console.log(`[${m.type()}] ${m.text()}`));
		for (const l of h.console ? dig.console : []) console.log(`[${l.type}] ${l.text}`);
		if (h.settle) await dig.page.waitForTimeout(h.settle);
		if (h.keys.length) await dig.keys(...h.keys);
		if (h.click) await dig.page.mouse.click(h.click[0], h.click[1]);
		if (h.frames != null) await dig.exit(h.timeout + h.frames * 100);
		else if (h.keys.length || h.click) await dig.page.waitForTimeout(300);
		if (h.screenshot) {
			const png = await dig.shot(h.screenshot);
			console.error(`→ wrote ${h.screenshot} (${png.width}×${png.height})`);
			if (h.stats) console.log(JSON.stringify(stats(png)));
		}
		const trouble = dig.trouble();
		if (trouble.length) {
			code = 1;
			console.error("console trouble:");
			for (const l of trouble) console.error(`  [${l.type}] ${l.text}`);
		}
		for (const m of dig.metrics().slice(-1)) console.error(`→ ${m.line}`);
		if (h.headed) {
			console.error("(headed: window stays up — Ctrl-C to end)");
			await new Promise(() => {});
		}
	} catch (e) {
		code = 1;
		console.error(e.message ?? e);
	} finally {
		await browser.close();
		await server?.close();
	}
	process.exit(code);
}

main();
