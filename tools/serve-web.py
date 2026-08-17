#!/usr/bin/env python3
"""Dev server for the web bundle (`just web` / `web-live` / `play-web`).

Two listeners over one directory:

- HTTP on 127.0.0.1 — the unchanged desktop loop. Loopback is a secure
  context, so plain http is enough there.
- HTTPS on 0.0.0.0 — the phone-on-the-LAN loop. WebGPU only exists in a
  secure context, and a phone reaching this machine by LAN IP is not on
  loopback, so it must arrive over TLS. The certificate is self-signed
  (accept the browser warning once); it persists in data/dev-web-cert and
  is only regenerated when the LAN IP stops matching its SANs, so the
  phone's one-time trust survives across runs.

`--master-proxy URL` reverse-proxies `/master/*` on both listeners to a
local master. The phone needs it: an https page may not fetch a plain
http://LAN-IP master (mixed content), but the same master reached through
the page's own origin is same-origin and clean. The printed phone URL
carries `?master=<origin>/master` for exactly that reason.
"""

import argparse
import functools
import http.server
import mimetypes
import os
import socket
import ssl
import subprocess
import sys
import threading
import urllib.error
import urllib.request

CERT_DIR = "data/dev-web-cert"
HOP_BY_HOP = {
	"connection",
	"keep-alive",
	"proxy-authenticate",
	"proxy-authorization",
	"te",
	"trailers",
	"transfer-encoding",
	"upgrade",
}


def lan_ip():
	"""The address a LAN peer reaches this machine at. The connect never
	sends a packet — it just forces the OS to pick the outbound route."""
	try:
		with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
			s.connect(("8.8.8.8", 80))
			return s.getsockname()[0]
	except OSError:
		return None


def ensure_cert(ip):
	"""A persistent self-signed cert whose SANs cover the current LAN IP.
	Regenerating on every run would make the phone re-accept the warning
	every run, so it is reused until the IP falls out of it."""
	cert = os.path.join(CERT_DIR, "cert.pem")
	key = os.path.join(CERT_DIR, "key.pem")
	if os.path.exists(cert) and os.path.exists(key):
		sans = subprocess.run(
			["openssl", "x509", "-in", cert, "-noout", "-ext", "subjectAltName"],
			capture_output=True,
			text=True,
		).stdout
		if f"IP Address:{ip}" in sans:
			return cert, key
	os.makedirs(CERT_DIR, exist_ok=True)
	subprocess.run(
		[
			"openssl", "req", "-x509", "-newkey", "rsa:2048",
			"-keyout", key, "-out", cert, "-days", "365", "-nodes",
			"-subj", "/CN=DigBlock dev",
			"-addext", f"subjectAltName=DNS:localhost,IP:127.0.0.1,IP:{ip}",
		],
		check=True,
		capture_output=True,
	)
	print(f"→ new self-signed cert for {ip} (accept the warning once per device)")
	return cert, key


class Handler(http.server.SimpleHTTPRequestHandler):
	master_proxy = None

	def log_message(self, fmt, *args):
		pass  # the dev loop doesn't need an access log

	# ---- /master/* reverse proxy -----------------------------------
	def _proxy(self):
		target = self.master_proxy + self.path[len("/master"):]
		body = None
		length = int(self.headers.get("Content-Length") or 0)
		if length:
			body = self.rfile.read(length)
		req = urllib.request.Request(target, data=body, method=self.command)
		for h in ("Content-Type", "Authorization", "Accept"):
			if self.headers.get(h):
				req.add_header(h, self.headers[h])
		try:
			# Redirects are passed THROUGH, not followed: a sign-in flow's
			# Location must reach the browser.
			opener = urllib.request.build_opener(NoRedirect)
			resp = opener.open(req, timeout=15)
		except urllib.error.HTTPError as e:
			resp = e
		except OSError as e:
			self.send_error(502, f"master unreachable: {e}")
			return
		payload = resp.read()
		self.send_response(resp.status)
		for k, v in resp.headers.items():
			if k.lower() not in HOP_BY_HOP and k.lower() != "content-length":
				self.send_header(k, v)
		self.send_header("Content-Length", str(len(payload)))
		self.end_headers()
		self.wfile.write(payload)

	def _route(self, fallback):
		if self.master_proxy and self.path.startswith("/master/"):
			self._proxy()
		else:
			fallback()

	def do_GET(self):
		self._route(super().do_GET)

	def do_HEAD(self):
		self._route(super().do_HEAD)

	def do_POST(self):
		if self.master_proxy and self.path.startswith("/master/"):
			self._proxy()
		else:
			self.send_error(405)


class NoRedirect(urllib.request.HTTPRedirectHandler):
	def redirect_request(self, *args, **kwargs):
		return None


def serve(bind, port, directory, tls=None):
	handler = functools.partial(Handler, directory=directory)
	httpd = http.server.ThreadingHTTPServer((bind, port), handler)
	if tls:
		ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
		ctx.load_cert_chain(*tls)
		httpd.socket = ctx.wrap_socket(httpd.socket, server_side=True)
	httpd.serve_forever()


def main():
	ap = argparse.ArgumentParser()
	ap.add_argument("--dir", default="target/web")
	ap.add_argument("--http-port", type=int, default=8080)
	ap.add_argument("--https-port", type=int, default=8443)
	ap.add_argument("--master-proxy", help="local master URL to mirror under /master/")
	ap.add_argument("--query", default="", help="query string for the printed URLs")
	ap.add_argument(
		"--phone-query",
		default=None,
		help="query for the phone URL (defaults to --query); the token "
		"{master} expands to the phone origin's /master proxy",
	)
	ap.add_argument(
		"--print-lan-ip",
		action="store_true",
		help="print the LAN IP and exit (the play-web recipe feeds it to "
		"the game server's --advertise-host, so there is one detector)",
	)
	args = ap.parse_args()
	if args.print_lan_ip:
		print(lan_ip() or "127.0.0.1")
		return
	mimetypes.add_type("application/wasm", ".wasm")
	Handler.master_proxy = args.master_proxy.rstrip("/") if args.master_proxy else None

	print(f"→ http://127.0.0.1:{args.http_port}/{args.query}")
	ip = lan_ip()
	tls = None
	if ip:
		try:
			tls = ensure_cert(ip)
		except (subprocess.CalledProcessError, OSError) as e:
			print(f"(no LAN https listener — cert generation failed: {e})", file=sys.stderr)
	if tls:
		origin = f"https://{ip}:{args.https_port}"
		phone_query = args.phone_query if args.phone_query is not None else args.query
		phone_query = phone_query.replace("{master}", f"{origin}/master")
		print(f"→ {origin}/{phone_query}   (phone on the same wifi)")
		threading.Thread(
			target=serve,
			args=("0.0.0.0", args.https_port, args.dir, tls),
			daemon=True,
		).start()
	serve("127.0.0.1", args.http_port, args.dir)


if __name__ == "__main__":
	try:
		main()
	except KeyboardInterrupt:
		pass
