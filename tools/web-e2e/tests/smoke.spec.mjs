// The page boots, renders, ends when told, and says so when it cannot.
import { test, expect, expectRendered, diff, detail, stats } from "./fixtures.mjs";

/** How long a screen is given to open, and the length of the control
 *  interval it is measured against. Long enough for several frames at any
 *  rate the suite runs at, short enough that the world barely moves. */
const STEP_MS = 400;

test("a capture run reaches its frame count and renders terrain", async ({ dig }) => {
	const d = await dig({ seed: 7, tod: 60, frames: 90, fixeddt: 1 / 60 });
	await d.exit();
	const png = await d.shot(test.info().outputPath("terrain.png"));
	expectRendered(png);
	// The floor above is only that: a sky-only frame passes it, and so
	// does a garbled decode. Terrain is what carries block-scale
	// STRUCTURE, and this pose puts the horizon across the middle of the
	// frame: the sky over it stays a smooth gradient whatever the hour,
	// so the band AT the horizon running several times the sky's local
	// contrast is the part that stops being true when no chunk arrives.
	// Measured up the frame and not down it because the HUD sits in the
	// bottom tenth, and its edges are structure the world did not draw —
	// a camera pitched at the sky still scores there.
	const { width: w, height: h } = png;
	const sky = detail(png, { region: [0, 0, w, Math.round(h * 0.2)] });
	const horizon = detail(png, { region: [0, Math.round(h * 0.4), w, Math.round(h * 0.6)] });
	expect(horizon, `the horizon band (${horizon.toFixed(2)}) should be textured beside the sky (${sky.toFixed(2)})`)
		.toBeGreaterThan(sky * 4);
	expect(d.trouble(), "console trouble").toEqual([]);
});

test("the scenes the desktop can pose, the browser can pose", async ({ dig }) => {
	// Each arm is a query-only knob the page had no spelling for before
	// knobs::ALL; a typo in the table reaches the console as a panic.
	for (const params of [{ vault: true }, { node: "leystone" }, { screen: "alchemy" }, { screen: "spellbook" }, { ore: 2, siphon: 50 }]) {
		const d = await dig({ seed: 7, frames: 45, ...params });
		await d.exit();
		expectRendered(await d.shot(test.info().outputPath(`${Object.keys(params).join("-")}.png`)), JSON.stringify(params));
		expect(d.trouble(), JSON.stringify(params)).toEqual([]);
	}
});

test("an unknown query key is refused, not ignored", async ({ browser, baseURL }) => {
	const { boot } = await import("../lib/digblock.mjs");
	await expect(boot(browser, baseURL, { seed: 7, frames: 10, vualt: true }, { timeout: 30_000 })).rejects.toThrow(/vualt/);
});

test("K opens the alchemy screen on a menu-less interactive run and Escape closes it", async ({ dig }) => {
	const d = await dig({ seed: 7, play: true, mute: true, master: "" });
	await d.page.waitForTimeout(1500);
	// An interactive run has no fixed dt, so a whole-frame delta says only
	// that TIME passed: the sun moves, the water ripples and late chunks
	// land. The control interval is the same wait with no keypress in it —
	// the world's own drift, and the floor the keypress has to clear — and
	// what a screen does is specific: one panel over one dim (ui/theme.rs),
	// so the frame gets DARKER and gets its brightness back on Escape.
	const settled = await d.shot();
	await d.page.waitForTimeout(STEP_MS);
	const before = await d.shot();
	const drift = diff(settled, before);
	await d.keys("k");
	await d.page.waitForTimeout(STEP_MS);
	const open = await d.shot(test.info().outputPath("alchemy-open.png"));
	expect(diff(before, open), `the keypress must move the frame further than the world drifts on its own (${drift.toFixed(2)})`)
		.toBeGreaterThan(drift * 4 + 4);
	const lit = stats(before).mean;
	const dimmed = stats(open).mean / lit;
	expect(dimmed, "an open screen dims the world behind it").toBeLessThan(0.8);
	await d.keys("Escape");
	await d.page.waitForTimeout(STEP_MS);
	const closed = await d.shot();
	expect(stats(closed).mean / lit, "Escape lifts the dim again").toBeGreaterThan(0.9);
	expect(d.trouble()).toEqual([]);
});

test("the canvas backing is the CSS box times the device ratio, at 1x and 2x", async ({ dig }) => {
	// The desktop's `--size` holds the surface equal across platforms; here
	// that is the viewport × DPR, and a backing that disagrees renders the
	// UI for the wrong surface and stretches it (the 2× emulation quirk
	// lib/digblock.mjs corrects).
	for (const dpr of [1, 2]) {
		const d = await dig({ seed: 7, frames: 5 }, { size: [960, 600], dpr });
		await d.exit();
		const c = await d.canvas();
		expect(c.dpr).toBe(dpr);
		expect(c.backing).toEqual([c.css[0] * dpr, c.css[1] * dpr]);
		const png = await d.shot();
		expect([png.width, png.height]).toEqual(c.backing);
	}
});
