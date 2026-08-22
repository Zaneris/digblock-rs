// The bundle, served. A static server over `target/web` with nothing the
// dev loop's `tools/serve-web.py` has (no LAN listener, no cert, no master
// proxy): the harness runs on loopback, which is already a secure context,
// and a second python dependency for a node tool is one more thing to be
// missing. `application/wasm` is the one MIME that matters — without it
// `instantiateStreaming` refuses the module.
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
export const BUNDLE = path.join(REPO, "target/web");

const MIME = {
	".html": "text/html; charset=utf-8",
	".js": "text/javascript",
	".wasm": "application/wasm",
	".json": "application/json",
	".png": "image/png",
	".ico": "image/x-icon",
};

/** Start serving `dir` on 127.0.0.1:`port` (0 = any free port). Resolves to the origin. */
export function serve({ dir = BUNDLE, port = 0 } = {}) {
	if (!fs.existsSync(path.join(dir, "index.html"))) {
		throw new Error(`no web bundle at ${dir} — run \`just build-web\` first`);
	}
	const server = http.createServer((req, res) => {
		const url = new URL(req.url, "http://x");
		// Chrome asks for one on every navigation; a 404 in the console
		// would read as trouble to the driver's filter.
		if (url.pathname === "/favicon.ico") return res.writeHead(204).end();
		let file = path.join(dir, decodeURIComponent(url.pathname));
		if (url.pathname.endsWith("/")) file = path.join(file, "index.html");
		if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
			res.writeHead(404).end("not found");
			return;
		}
		const stat = fs.statSync(file);
		res.writeHead(200, {
			"Content-Type": MIME[path.extname(file)] ?? "application/octet-stream",
			"Content-Length": stat.size,
			"Cache-Control": "no-store",
		});
		if (req.method === "HEAD") return res.end();
		fs.createReadStream(file).pipe(res);
	});
	return new Promise((resolve, reject) => {
		server.on("error", reject);
		server.listen(port, "127.0.0.1", () => {
			const origin = `http://127.0.0.1:${server.address().port}`;
			resolve({ origin, close: () => new Promise((r) => server.close(r)) });
		});
	});
}

// `node lib/serve.mjs [port]` — what playwright.config's webServer runs.
if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
	const { origin } = await serve({ port: Number(process.argv[2] ?? 8090) });
	console.log(`serving ${BUNDLE} at ${origin}`);
}
