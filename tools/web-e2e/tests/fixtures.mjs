// `test.extend` with the one thing every spec needs: a booted game. The
// browser is shared across a file; each `dig()` call is a fresh context,
// so nothing a test saved to localStorage leaks into the next.
import { test as base, expect } from "@playwright/test";
import { boot, launch, stats, diff } from "../lib/digblock.mjs";

export const test = base.extend({
	browser: [async ({}, use) => {
		const b = await launch();
		await use(b);
		await b.close();
	}, { scope: "worker" }],
	/** `dig(params, opts)` — boot to the first frame; contexts close with the test. */
	dig: async ({ browser, baseURL }, use, testInfo) => {
		const open = [];
		await use(async (params = {}, opts = {}) => {
			const d = await boot(browser, baseURL, params, opts);
			open.push(d);
			return d;
		});
		for (const d of open) {
			if (testInfo.status !== testInfo.expectedStatus) {
				await testInfo.attach("console", { body: d.console.map((l) => `[${l.type}] ${l.text}`).join("\n"), contentType: "text/plain" });
				await d.shot(testInfo.outputPath("last.png")).catch(() => {});
			}
			await d.page.context().close();
		}
	},
});

export { expect, stats, diff };

/** The canvas is showing SOMETHING: not black, not one flat colour. */
export function expectRendered(png, what = "the frame") {
	const s = stats(png);
	expect(s.darkShare, `${what} is mostly black`).toBeLessThan(0.9);
	expect(s.colours, `${what} is one flat colour`).toBeGreaterThan(64);
	return s;
}
