#!/usr/bin/env python3
"""Drive the dedicated servers listed in fleet.toml over ssh (`just fleet --help`).

The inventory is authoritative: a server's compose.yml on its host is
deploy/compose.yml with the inventory's values substituted, and `apply`
pushes that rendering — so the kit stays the one template and a kit change
reaches the fleet on the next apply. Nothing here takes a password: `setup`
installs a key through ssh-copy-id, which prompts, and every later call is
BatchMode. Stdlib only, so `python3 tools/fleet/fleet.py` is the whole install.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
KIT = ROOT / "deploy" / "compose.yml"
INVENTORY = Path(__file__).with_name("fleet.toml")
FETCH_DIR = ROOT / "data" / "fleet-backups"

IMAGE = "ghcr.io/zaneris/digblock-server"
MASTER_URL = "https://master.digblock.net"
# The master's block, as the kit quotes it: an operator's tool cannot derive
# it, so `the_masters_port_block_mirrors_ours` reads this line.
PORT_BLOCK = (4433, 4499)
KEY = Path.home() / ".ssh" / "id_ed25519"
BACKUP_DIR = "digblock-backups"
BACKUPS_KEPT = 5
WATCH_SECS = 120
WATCH_POLL_SECS = 3
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]

HOST_KEYS = {"addr", "user"}
SERVER_KEYS = {"host", "dir", "port", "name", "seed", "admins", "advertise_host", "version", "archive_world", "adopt_terrain"}
# Row keys that are a one-off DECISION rather than configuration: each is inert until its own
# reason applies, so leaving one set costs nothing, but `apply` says so every time.
DECISIONS = ("archive_world", "adopt_terrain")


class Fatal(Exception):
	pass


@dataclass(frozen=True)
class Host:
	name: str
	addr: str
	user: str

	@property
	def target(self) -> str:
		return f"{self.user}@{self.addr}"


@dataclass(frozen=True)
class Values:
	"""What one compose.yml says, in the six slots the kit leaves to the operator."""

	port: int
	tag: str
	name: str
	seed: str | None = None
	admins: tuple[str, ...] = ()
	advertise_host: str | None = None
	# The two world-replacement levers (DECISIONS). Separate, because one set to recover a world
	# this build cannot read must not still be armed at the next terrain release.
	archive_world: bool = False
	adopt_terrain: bool = False

	def same_world(self, other: Values) -> bool:
		"""Whether the two name the same world. Seeds compare NUMERICALLY, so "6" and "6.0" are
		one world — but a remote file is not necessarily one we rendered, and a hand-edited seed
		that is not a number must be a refusal rather than a traceback out of `diff`."""

		def value(v: Values) -> float | None:
			if v.seed is None:
				return None
			try:
				return float(v.seed)
			except ValueError:
				raise Fatal(f"seed {v.seed!r} in a compose.yml is not a number") from None

		return value(self) == value(other)

	def changes(self, other: Values) -> list[str]:
		"""Field-by-field differences, worded as `field: mine -> other`."""
		out = []
		for name in ("port", "tag", "name", "seed", "admins", "advertise_host", *DECISIONS):
			a, b = getattr(self, name), getattr(other, name)
			if name == "seed":
				if self.same_world(other):
					continue
			elif a == b:
				continue
			out.append(f"{name}: {a!r} -> {b!r}")
		return out


@dataclass(frozen=True)
class Server:
	name: str
	host: Host
	dir: str
	values: Values

	@property
	def label(self) -> str:
		return f"{self.name} ({self.host.name}/{self.dir}:{self.values.port})"


@dataclass
class Inventory:
	default_version: str
	hosts: dict[str, Host] = field(default_factory=dict)
	servers: dict[str, Server] = field(default_factory=dict)

	def select(self, names: list[str]) -> list[Server]:
		"""No names is every server; a name may be a server's or a host's."""
		if not names:
			return list(self.servers.values())
		out: list[Server] = []
		for n in names:
			if n in self.servers:
				out.append(self.servers[n])
			elif n in self.hosts:
				out.extend(s for s in self.servers.values() if s.host.name == n)
			else:
				raise Fatal(f"{n!r} is neither a server nor a host in {INVENTORY.name}")
		seen: set[str] = set()
		return [s for s in out if not (s.name in seen or seen.add(s.name))]

	def hosts_of(self, servers: list[Server]) -> list[Host]:
		seen: set[str] = set()
		return [s.host for s in servers if not (s.host.name in seen or seen.add(s.host.name))]


# ---------------------------------------------------------------- inventory


def seed_text(seed) -> str | None:
	"""A TOML integer renders bare so the world directory name matches what
	an operator typing `DIGBLOCK_SEED: "6"` by hand would have got."""
	if seed is None:
		return None
	if isinstance(seed, bool) or not isinstance(seed, (int, float, str)):
		raise Fatal(f"seed must be a number, not {seed!r}")
	text = str(seed)
	try:
		float(text)
	except ValueError:
		raise Fatal(f"seed {seed!r} is not a number") from None
	return text


def build_inventory(data: dict) -> Inventory:
	unknown = set(data) - {"version", "hosts", "servers"}
	if unknown:
		raise Fatal(f"unknown top-level key(s) {sorted(unknown)}")
	default_version = data.get("version")
	if not isinstance(default_version, str) or not default_version:
		raise Fatal('a fleet-wide `version = "x.y.z"` is required')
	inv = Inventory(default_version)
	for name, row in (data.get("hosts") or {}).items():
		unknown = set(row) - HOST_KEYS
		if unknown:
			raise Fatal(f"host {name!r}: unknown key(s) {sorted(unknown)}")
		missing = HOST_KEYS - set(row)
		if missing:
			raise Fatal(f"host {name!r}: missing {sorted(missing)}")
		inv.hosts[name] = Host(name, str(row["addr"]), str(row["user"]))
	claimed_ports: set[tuple[str, int]] = set()
	claimed_dirs: set[tuple[str, str]] = set()
	for name, row in (data.get("servers") or {}).items():
		unknown = set(row) - SERVER_KEYS
		if unknown:
			raise Fatal(f"server {name!r}: unknown key(s) {sorted(unknown)}")
		missing = {"host", "dir", "port", "name"} - set(row)
		if missing:
			raise Fatal(f"server {name!r}: missing {sorted(missing)}")
		host = inv.hosts.get(row["host"])
		if host is None:
			raise Fatal(f"server {name!r}: host {row['host']!r} is not in [hosts]")
		port = row["port"]
		if isinstance(port, bool) or not isinstance(port, int) or not PORT_BLOCK[0] <= port <= PORT_BLOCK[1]:
			raise Fatal(f"server {name!r}: port {port!r} is outside the master's block {PORT_BLOCK[0]}-{PORT_BLOCK[1]}")
		if (host.name, port) in claimed_ports:
			raise Fatal(f"server {name!r}: port {port} is already claimed on host {host.name!r}")
		claimed_ports.add((host.name, port))
		directory = str(row["dir"]).strip("/")
		if not directory or "/" in directory or directory.startswith("."):
			raise Fatal(f"server {name!r}: dir {row['dir']!r} must be one folder under the user's home")
		if (host.name, directory) in claimed_dirs:
			raise Fatal(f"server {name!r}: dir {directory!r} is already claimed on host {host.name!r}")
		claimed_dirs.add((host.name, directory))
		admins = row.get("admins", [])
		if not isinstance(admins, list) or not all(isinstance(a, str) and a for a in admins):
			raise Fatal(f"server {name!r}: admins must be a list of account ids")
		decisions = {}
		for key in DECISIONS:
			v = row.get(key, False)
			if not isinstance(v, bool):
				raise Fatal(f"server {name!r}: {key} must be true or false, not {v!r}")
			decisions[key] = v
		values = Values(
			port=port,
			tag=str(row.get("version", default_version)),
			name=str(row["name"]),
			seed=seed_text(row.get("seed")),
			admins=tuple(admins),
			advertise_host=row.get("advertise_host") or None,
			**decisions,
		)
		inv.servers[name] = Server(name, host, directory, values)
	return inv


def load_inventory(path: Path = INVENTORY) -> Inventory:
	try:
		data = tomllib.loads(path.read_text())
	except FileNotFoundError:
		raise Fatal(f"{path} does not exist") from None
	except tomllib.TOMLDecodeError as e:
		raise Fatal(f"{path}: {e}") from None
	return build_inventory(data)


def toml_rows(inv: Inventory, servers: dict[str, tuple[Host, str, Values]]) -> str:
	"""The inventory as TOML text — `adopt`'s output, in the shape `load_inventory` reads."""
	lines = [f'version = "{inv.default_version}"', ""]
	for h in inv.hosts.values():
		lines += [f"[hosts.{h.name}]", f'addr = "{h.addr}"', f'user = "{h.user}"', ""]
	for name, (host, directory, v) in servers.items():
		lines += [f"[servers.{name}]", f'host = "{host.name}"', f'dir = "{directory}"', f"port = {v.port}", f"name = {json.dumps(v.name)}"]
		if v.seed is not None:
			lines.append(f"seed = {v.seed}")
		if v.admins:
			lines.append("admins = [" + ", ".join(json.dumps(a) for a in v.admins) + "]")
		if v.advertise_host:
			lines.append(f"advertise_host = {json.dumps(v.advertise_host)}")
		for key in DECISIONS:
			if getattr(v, key):
				lines.append(f"{key} = true")
		if v.tag != inv.default_version:
			lines.append(f'version = "{v.tag}"')
		lines.append("")
	return "\n".join(lines)


# ------------------------------------------------------------ render / parse

PORT_LINE = re.compile(r'^x-port: &port "(\d+)"$')
IMAGE_LINE = re.compile(rf"^(\s*)image: {re.escape(IMAGE)}:(\S+)$")
ENV_LINE = re.compile(r'^(\s*)(#?)(DIGBLOCK_(?:SERVER_NAME|ADVERTISE_HOST|SEED|ADMIN_IDS|ARCHIVE_WORLD|ADOPT_TERRAIN)): "(.*)"$')
OPTIONAL = {"DIGBLOCK_ADVERTISE_HOST", "DIGBLOCK_SEED", "DIGBLOCK_ADMIN_IDS", "DIGBLOCK_ARCHIVE_WORLD", "DIGBLOCK_ADOPT_TERRAIN"}


def _slots(text: str, *, missing_ok: frozenset[str] = frozenset()) -> dict[str, int]:
	"""Line index of each substitutable line: at most one apiece, and required unless in `missing_ok`."""
	found: dict[str, list[int]] = {}
	for i, line in enumerate(text.splitlines()):
		if PORT_LINE.match(line):
			found.setdefault("port", []).append(i)
		elif IMAGE_LINE.match(line):
			found.setdefault("image", []).append(i)
		elif m := ENV_LINE.match(line):
			found.setdefault(m.group(3), []).append(i)
	expected = {"port", "image", "DIGBLOCK_SERVER_NAME", *OPTIONAL}
	for key in expected:
		n = len(found.get(key, []))
		if n > 1 or (n == 0 and key not in missing_ok):
			raise Fatal(f"not a kit-shaped compose.yml: {key} appears {n} times, expected once")
	return {k: v[0] for k, v in found.items()}


def parse(text: str) -> Values:
	# A host may hold a file pushed by an older kit that predates a newer optional slot, and apply
	# has to be able to read it to replace it — so an absent optional reads as the commented slot,
	# while the local kit itself stays fully slotted (render's _slots call is the strict one).
	lines = text.splitlines()
	slots = _slots(text, missing_ok=frozenset(OPTIONAL))
	port = int(PORT_LINE.match(lines[slots["port"]]).group(1))
	tag = IMAGE_LINE.match(lines[slots["image"]]).group(2)

	def env(key: str) -> str | None:
		if key not in slots:
			return None
		m = ENV_LINE.match(lines[slots[key]])
		return None if m.group(2) else m.group(4)

	admins = env("DIGBLOCK_ADMIN_IDS")
	return Values(
		port=port,
		tag=tag,
		name=env("DIGBLOCK_SERVER_NAME") or "",
		seed=env("DIGBLOCK_SEED"),
		admins=tuple(a.strip() for a in admins.split(",") if a.strip()) if admins else (),
		advertise_host=env("DIGBLOCK_ADVERTISE_HOST"),
		# The server parses these itself; here only set-and-true counts as on,
		# so a commented slot and an explicit "false" read the same.
		archive_world=(env("DIGBLOCK_ARCHIVE_WORLD") or "").lower() == "true",
		adopt_terrain=(env("DIGBLOCK_ADOPT_TERRAIN") or "").lower() == "true",
	)


def render(v: Values, kit: str | None = None) -> str:
	"""The kit with `v` written into its slots; an absent optional keeps the kit's commented example."""
	text = KIT.read_text() if kit is None else kit
	lines = text.splitlines()
	slots = _slots(text)
	lines[slots["port"]] = f'x-port: &port "{v.port}"'
	indent = IMAGE_LINE.match(lines[slots["image"]]).group(1)
	lines[slots["image"]] = f"{indent}image: {IMAGE}:{v.tag}"
	wanted = {
		"DIGBLOCK_SERVER_NAME": v.name,
		"DIGBLOCK_ADVERTISE_HOST": v.advertise_host,
		"DIGBLOCK_SEED": v.seed,
		"DIGBLOCK_ADMIN_IDS": ",".join(v.admins) if v.admins else None,
		# Off is the slot left commented, so a row that has not decided
		# renders the kit's own example and the host carries no switch.
		"DIGBLOCK_ARCHIVE_WORLD": "true" if v.archive_world else None,
		"DIGBLOCK_ADOPT_TERRAIN": "true" if v.adopt_terrain else None,
	}
	for key, value in wanted.items():
		i = slots[key]
		m = ENV_LINE.match(lines[i])
		if value is None:
			if key not in OPTIONAL:
				raise Fatal(f"{key} has no value")
			# Keep the kit's own example, commented, so the file still teaches.
			lines[i] = f'{m.group(1)}#{key}: "{m.group(4)}"' if not m.group(2) else lines[i]
		else:
			if '"' in value or "\n" in value:
				raise Fatal(f"{key} value {value!r} cannot be quoted into YAML")
			lines[i] = f'{m.group(1)}{key}: "{value}"'
	return "\n".join(lines) + "\n"


def classify(line: str) -> str | None:
	"""What one server log line says about a boot: live, failed, archived, or nothing.

	Substring matching over free-text logs is what this is, so the live test is anchored on the
	registration line's own tail rather than on a phrase that also appears inside a REFUSAL:
	`conn N: refused (server not yet registered with master)` contains "registered with master",
	and a player reconnecting during an upgrade would otherwise report the boot as a success and
	stop the watch.
	"""
	if "registered with master" in line and "(listed at " in line:
		return "live"
	if "world archived" in line or ("archiving" in line and ".bak" in line):
		return "archived"
	if "UNREACHABLE" in line or "failed to" in line or "error:" in line.lower():
		return "failed"
	return None


# ---------------------------------------------------------------- remoting


def ssh(host: Host, script: str, *, check: bool = True, capture: bool = True) -> subprocess.CompletedProcess:
	cmd = ["ssh", *SSH_OPTS, host.target, script]
	r = subprocess.run(cmd, text=True, capture_output=capture)
	if check and r.returncode != 0:
		detail = (r.stderr or "").strip() if capture else ""
		raise Fatal(f"{host.name}: ssh failed ({r.returncode}){': ' + detail if detail else ''}")
	return r


def in_dir(server: Server, script: str) -> str:
	return f'cd "$HOME"/{shlex.quote(server.dir)} && {script}'


def compose(server: Server, args: str, *, check: bool = True, capture: bool = True) -> subprocess.CompletedProcess:
	return ssh(server.host, in_dir(server, f"docker compose {args}"), check=check, capture=capture)


def remote_compose_text(server: Server) -> str | None:
	r = ssh(server.host, in_dir(server, "cat compose.yml"), check=False)
	if r.returncode != 0:
		if "No such file" in r.stderr:
			return None
		raise Fatal(f"{server.label}: {r.stderr.strip()}")
	return r.stdout


def push_compose(server: Server, text: str) -> None:
	"""scp to a temp name, then mv — a half-copied file never sits where `up` reads it."""
	tmp = f"compose.yml.fleet-{os.getpid()}"
	remote = f"{server.host.target}:{server.dir}/{tmp}"
	ssh(server.host, f'mkdir -p "$HOME"/{shlex.quote(server.dir)}')
	with tempfile.NamedTemporaryFile("w", suffix=".yml", delete_on_close=False) as local:
		local.write(text)
		local.close()
		r = subprocess.run(["scp", "-q", *SSH_OPTS, local.name, remote], text=True, capture_output=True)
	if r.returncode != 0:
		raise Fatal(f"{server.label}: scp failed: {r.stderr.strip()}")
	ssh(server.host, in_dir(server, f"mv {tmp} compose.yml"))


def ps_entries(text: str) -> list[dict]:
	"""`compose ps --format json` is one object per line on current Compose and an array on older ones."""
	out: list[dict] = []
	for line in text.splitlines():
		if not line.strip():
			continue
		parsed = json.loads(line)
		out.extend(parsed if isinstance(parsed, list) else [parsed])
	return out


def container_state(server: Server) -> tuple[str | None, str]:
	"""(container id, state) of the service's container; (None, "absent") when there is none."""
	entries = ps_entries(compose(server, "ps -a --format json", check=False).stdout)
	if not entries:
		return None, "absent"
	return entries[0].get("ID"), entries[0].get("State", "?")


def watch_boot(server: Server, since: str) -> str:
	"""Tail the container's log from `since` until it registers, fails, or the clock runs out."""
	deadline = time.monotonic() + WATCH_SECS
	seen: set[str] = set()
	verdict = "timeout"
	while time.monotonic() < deadline:
		r = compose(server, f"logs --no-log-prefix --since {shlex.quote(since)}", check=False)
		for line in r.stdout.splitlines():
			kind = classify(line)
			if kind and line not in seen:
				seen.add(line)
				print(f"    {line.strip()}")
				if kind == "archived":
					print(f"    !! {server.label}: the journal was ARCHIVED and the world started fresh — see {BACKUP_DIR} on the host")
				if kind in ("live", "failed"):
					verdict = kind
		if verdict in ("live", "failed"):
			break
		time.sleep(WATCH_POLL_SECS)
	if verdict == "timeout":
		print(f"    no verdict from the log in {WATCH_SECS}s — `just fleet logs {server.name}`")
	return verdict


def backup(server: Server, fetch: bool) -> str:
	script = in_dir(
		server,
		f'mkdir -p "$HOME"/{BACKUP_DIR} && f="$HOME"/{BACKUP_DIR}/{server.values.port}-$(date -u +%Y%m%dT%H%M%SZ).tgz'
		f' && sudo -n tar czf "$f" data && sudo -n chown "$(id -u):$(id -g)" "$f"'
		f' && ls -1t "$HOME"/{BACKUP_DIR}/{server.values.port}-*.tgz | tail -n +{BACKUPS_KEPT + 1} | xargs -r rm --'
		f' && echo "$f"',
	)
	path = ssh(server.host, script).stdout.strip()
	print(f"  {server.label}: backed up to {path}")
	if fetch:
		FETCH_DIR.mkdir(parents=True, exist_ok=True)
		local = FETCH_DIR / f"{server.host.name}-{Path(path).name}"
		r = subprocess.run(["scp", "-q", *SSH_OPTS, f"{server.host.target}:{path}", str(local)], text=True, capture_output=True)
		if r.returncode != 0:
			raise Fatal(f"{server.label}: fetch failed: {r.stderr.strip()}")
		print(f"    fetched to {local.relative_to(ROOT)}")
	return path


def master_listing() -> dict[str, dict]:
	"""The master's public list keyed by `addr:port`; empty when the master is unreachable."""
	try:
		with urllib.request.urlopen(f"{MASTER_URL}/api/servers", timeout=10) as r:
			body = json.load(r)
	except Exception as e:  # noqa: BLE001 — a status table beats a traceback
		print(f"  (master unreachable: {e})", file=sys.stderr)
		return {}
	out = {}
	for s in body.get("servers", []):
		url = s.get("url", "")
		out[url.removeprefix("https://")] = s
	return out


# ---------------------------------------------------------------- commands


def cmd_setup(inv: Inventory, args) -> None:
	hosts = inv.hosts_of(inv.select(args.targets))
	if not KEY.exists():
		print(f"generating {KEY}")
		subprocess.run(["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(KEY)], check=True)
	for h in hosts:
		probe = subprocess.run(["ssh", *SSH_OPTS, h.target, "true"], capture_output=True)
		if probe.returncode != 0:
			print(f"{h.name}: installing the key on {h.target} (enter the host's password once)")
			r = subprocess.run(["ssh-copy-id", "-i", f"{KEY}.pub", h.target])
			if r.returncode != 0:
				raise Fatal(f"{h.name}: ssh-copy-id failed")
		version = ssh(h, "docker compose version --short").stdout.strip()
		print(f"{h.name}: key login ok, docker compose {version}")


def cmd_adopt(inv: Inventory, args) -> None:
	servers = inv.select(args.targets)
	adopted: dict[str, tuple[Host, str, Values]] = {}
	notes: list[str] = []
	for h in inv.hosts_of(servers):
		mine = [s for s in servers if s.host.name == h.name]
		known_dirs = {s.dir for s in inv.servers.values() if s.host.name == h.name}
		known_ports = {str(s.values.port) for s in inv.servers.values() if s.host.name == h.name}
		r = ssh(h, 'docker compose ls -a --format json; echo; ls -d "$HOME"/*/compose.yml 2>/dev/null')
		projects_json, _, dirs_text = r.stdout.partition("\n\n")
		for p in json.loads(projects_json or "[]"):
			if p.get("Name") not in known_ports:
				notes.append(f"{h.name}: stale compose project {p.get('Name')!r} ({p.get('Status')}) from {p.get('ConfigFiles')} — `docker compose -p {p.get('Name')} down --remove-orphans` clears it")
		for path in dirs_text.split():
			d = Path(path).parent.name
			if d not in known_dirs:
				notes.append(f"{h.name}: ~/{d}/compose.yml exists but no server row claims it")
		for s in mine:
			text = remote_compose_text(s)
			if text is None:
				notes.append(f"{s.label}: no compose.yml on the host")
				continue
			try:
				adopted[s.name] = (s.host, s.dir, parse(text))
			except Fatal as e:
				notes.append(f"{s.label}: {e}")
	print(toml_rows(inv, adopted), end="")
	for n in notes:
		print(f"# NOTE {n}")


def show_diff(server: Server, remote: str | None, rendered: str) -> tuple[list[str], bool]:
	"""Print the value summary and text diff; return (value changes, text differs)."""
	if remote is None:
		print(f"{server.label}: no compose.yml on the host yet — would create it")
		return ["(new)"], True
	changes = parse(remote).changes(server.values)
	differs = remote != rendered
	summary = "; ".join(changes) if changes else "values identical"
	if differs and not changes:
		summary += " (kit text differs)"
	print(f"{server.label}: {summary}")
	if differs:
		diff = difflib.unified_diff(remote.splitlines(), rendered.splitlines(), f"{server.host.name}:~/{server.dir}/compose.yml", "rendered", lineterm="")
		for line in diff:
			print(f"  {line}")
	return changes, differs


def cmd_diff(inv: Inventory, args) -> None:
	for s in inv.select(args.targets):
		show_diff(s, remote_compose_text(s), render(s.values))


def cmd_render(inv: Inventory, args) -> None:
	(server,) = inv.select([args.server])
	print(render(server.values), end="")


def cmd_apply(inv: Inventory, args) -> None:
	for s in inv.select(args.targets):
		remote = remote_compose_text(s)
		rendered = render(s.values)
		changes, differs = show_diff(s, remote, rendered)
		old = parse(remote) if remote is not None else None
		port_changed = old is not None and old.port != s.values.port
		seed_changed = old is not None and not old.same_world(s.values)
		if (port_changed or seed_changed) and not args.i_mean_it:
			why = "changing the port renames the compose project (the old one is torn down first)" if port_changed else "changing the seed STARTS A FRESH WORLD"
			print(f"  refused: {why}; rerun with --i-mean-it to do exactly that")
			continue
		# Said every apply, not only the one that flips it: these stay armed
		# until removed, and the whole point is that replacing a world is
		# never something an operator discovers afterwards.
		for key in DECISIONS:
			if getattr(s.values, key):
				print(f"  {key} is SET: this boot may move the world aside to a .vN.bak and start fresh")
		tag_changed = old is None or old.tag != s.values.tag
		was_running = container_state(s)[1] == "running"
		if port_changed:
			print("  tearing down the old project")
			compose(s, "down --remove-orphans")
		elif tag_changed and was_running:
			print("  stopping for the update")
			compose(s, "stop")
		if tag_changed and old is not None and was_running:
			backup(s, fetch=False)
		if differs:
			push_compose(s, rendered)
			print("  pushed compose.yml")
		if tag_changed:
			print(f"  pulling {IMAGE}:{s.values.tag}")
			compose(s, "pull --quiet")
		before = container_state(s)[0]
		since = ssh(s.host, "date -u +%Y-%m-%dT%H:%M:%SZ").stdout.strip()
		compose(s, "up -d --remove-orphans")
		after = container_state(s)[0]
		if after == before and was_running and not tag_changed:
			print("  up -d: nothing to recreate")
			continue
		print("  up -d: watching the boot")
		watch_boot(s, since)


def cmd_verb(verb: str):
	def run(inv: Inventory, args) -> None:
		for s in inv.select(args.targets):
			print(f"{s.label}: {verb}")
			since = ssh(s.host, "date -u +%Y-%m-%dT%H:%M:%SZ").stdout.strip()
			compose(s, verb if verb != "start" else "up -d")
			if verb != "stop":
				watch_boot(s, since)

	return run


def cmd_status(inv: Inventory, args) -> None:
	servers = inv.select(args.targets)
	listed = master_listing()
	rows = []
	for s in servers:
		r = ssh(s.host, in_dir(s, "docker compose ps -a --format json; echo; docker compose logs --no-log-prefix 2>/dev/null | head -1"), check=False)
		if r.returncode != 0:
			rows.append((s.name, f"{s.host.name}:{s.values.port}", s.dir, "ssh failed", "?", s.values.tag, "?"))
			continue
		ps_json, _, first_log = r.stdout.partition("\n\n")
		containers = ps_entries(ps_json)
		state = containers[0].get("State", "?") if containers else "absent"
		booted = re.search(r"digblock-server (\S+)", first_log)
		running = booted.group(1) if booted else "?"
		# The master lists a server at the address it ADVERTISED: the row's
		# advertise_host when it set one, the observed IP otherwise. Looking
		# only at the IP reported "not listed" for every healthy server with
		# a DNS name.
		entry = listed.get(f"{s.values.advertise_host or s.host.addr}:{s.values.port}")
		master = f"listed, {entry['players']} online, p{entry['protocolVersion']}/s{entry['simVersion']}" if entry else "not listed"
		pinned = s.values.tag + ("" if running == s.values.tag else "  <- drift")
		rows.append((s.name, f"{s.host.name}:{s.values.port}", s.dir, state, running, pinned, master))
	widths = [max(len(str(r[i])) for r in [("server", "host:port", "dir", "state", "running", "pinned", "master"), *rows]) for i in range(7)]
	for r in [("server", "host:port", "dir", "state", "running", "pinned", "master"), *rows]:
		print("  ".join(str(c).ljust(w) for c, w in zip(r, widths)).rstrip())


def cmd_logs(inv: Inventory, args) -> None:
	(server,) = inv.select([args.server])
	flags = f"-n {args.lines}" + (" -f" if args.follow else "")
	compose(server, f"logs {flags}", check=False, capture=False)


def cmd_backup(inv: Inventory, args) -> None:
	for s in inv.select(args.targets):
		if container_state(s)[1] == "running":
			print(f"  {s.label} is running: the archive may end mid-record; `stop` first for a clean one")
		backup(s, fetch=args.fetch)


def main(argv: list[str] | None = None) -> int:
	p = argparse.ArgumentParser(prog="fleet", description=__doc__.splitlines()[0])
	sub = p.add_subparsers(dest="cmd", required=True)

	def targets(sp, help_="server or host names; none means every server"):
		sp.add_argument("targets", nargs="*", help=help_)

	targets(sub.add_parser("setup", help="install this machine's ssh key on each host (prompts once per host)"))
	targets(sub.add_parser("adopt", help="print the inventory the hosts' compose files imply, and flag strays"))
	targets(sub.add_parser("diff", help="rendered compose.yml vs the host's copy"))
	sub.add_parser("render", help="print one server's rendered compose.yml").add_argument("server")
	sp = sub.add_parser("apply", help="push the rendering and `up -d`; a tag change stops, backs up and pulls first")
	targets(sp)
	sp.add_argument("--i-mean-it", action="store_true", help="allow a port or seed change (a seed change starts a fresh world)")
	for verb in ("start", "stop", "restart"):
		targets(sub.add_parser(verb, help=f"docker compose {verb}"))
	targets(sub.add_parser("status", help="container state, running vs pinned version, the master's view"))
	sp = sub.add_parser("logs", help="docker compose logs for one server")
	sp.add_argument("server")
	sp.add_argument("-f", "--follow", action="store_true")
	sp.add_argument("-n", "--lines", type=int, default=100)
	sp = sub.add_parser("backup", help=f"tar ./data into ~/{BACKUP_DIR} on the host, keeping the last {BACKUPS_KEPT}")
	targets(sp)
	sp.add_argument("--fetch", action="store_true", help=f"also copy the archive to {FETCH_DIR.relative_to(ROOT)}")
	args = p.parse_args(argv)

	handlers = {
		"setup": cmd_setup,
		"adopt": cmd_adopt,
		"diff": cmd_diff,
		"render": cmd_render,
		"apply": cmd_apply,
		"start": cmd_verb("start"),
		"stop": cmd_verb("stop"),
		"restart": cmd_verb("restart"),
		"status": cmd_status,
		"logs": cmd_logs,
		"backup": cmd_backup,
	}
	try:
		handlers[args.cmd](load_inventory(), args)
	except Fatal as e:
		print(f"fleet: {e}", file=sys.stderr)
		return 2
	except KeyboardInterrupt:
		return 130
	return 0


if __name__ == "__main__":
	sys.exit(main())
