// The page boots, renders, ends when told, and says so when it cannot.
import { test, expect, expectRendered, diff } from "./fixtures.mjs";

test("a capture run reaches its frame count and renders terrain", async ({ dig }) => {
	const d = await dig({ seed: 7, tod: 60, frames: 90, fixeddt: 1 / 60 });
	await d.exit();
	expectRendered(await d.shot(test.info().outputPath("terrain.png")));
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
	const before = await d.shot();
	await d.keys("k");
	await d.page.waitForTimeout(400);
	const open = await d.shot(test.info().outputPath("alchemy-open.png"));
	expect(diff(before, open), "the screen's dim should change the frame").toBeGreaterThan(4);
	await d.keys("Escape");
	await d.page.waitForTimeout(400);
	const closed = await d.shot();
	expect(diff(open, closed)).toBeGreaterThan(4);
	expect(d.trouble()).toEqual([]);
});
