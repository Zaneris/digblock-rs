// The smoke suite's rig. One worker: the browser holds the GPU, and two
// pages rendering at once turn every frame-count deadline into a coin
// toss. The bundle is served from target/web by lib/serve.mjs on a port
// the dev loop (8080/8443) never uses, so `just web` and the suite can
// coexist. `just build-web` is the caller's job — a stale bundle tests
// stale code, and this rig would rather refuse than rebuild silently.
import { defineConfig } from "@playwright/test";

export const PORT = 8090;

export default defineConfig({
	testDir: "./tests",
	timeout: 180_000,
	workers: 1,
	retries: 0,
	reporter: [["list"]],
	outputDir: "./test-results",
	use: { baseURL: `http://127.0.0.1:${PORT}` },
	webServer: {
		command: `node lib/serve.mjs ${PORT}`,
		url: `http://127.0.0.1:${PORT}/index.html`,
		reuseExistingServer: true,
		timeout: 10_000,
	},
});
