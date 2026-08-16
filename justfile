# DigBlock-rs build commands
#
# `just --list` shows only the LAST comment line above a recipe, so the
# one-line summary goes at the BOTTOM of a block and the detail above it.
# A block written top-down reads fine in the file and lists as a fragment.

# The scene, capture and attribution flags are catalogued in CLAUDE.md.
#
# Desktop client, debug build — opens at the title menu.
run *ARGS:
	cargo run -p digblock-desktop -- {{ARGS}}

# LEAVE THE WINDOW VISIBLE. macOS hands no surface texture to a window
# nobody can see, so a run whose screen locks — or that goes behind a
# full-screen window — stops producing frames and never reaches its frame
# count. It idles instead of failing, so a stalled bench looks like a slow
# one; `caffeinate -d just bench` if you mean to walk away. That is a
# swapchain property, and the reason `--offscreen` exists for captures — a
# bench cannot take that way out, because off the swapchain there is no
# present wait left to measure.
#
# Deterministic perf run (release build — dev numbers aren't representative). Needs a visible window.
bench *ARGS:
	cargo run --release -p digblock-desktop -- --bench {{ARGS}}

# Socket-free — it drives the real Sim through the same SimEvent seam the
# wtransport layer uses, so it is repeatable enough to compare two commits.
# Latency is NOT measured here (there is no wire); live RTT is on the debug
# overlay's `net` line. `--no-edits` isolates movement from broadcast
# fan-out.
#
# Deterministic netcode benchmark: sim-tick cost, wire bytes and allocations vs player count.
bench-net *ARGS:
	cargo run --release -p digblock-server --bin netbench -- {{ARGS}}

# Run natively for iterating on its code. The QUIC reachability probe is
# mandatory, so this needs `brew install libmsquic`; without it, use the
# container (`just play` builds and runs it automatically).
#
# The .NET master server: server list + one-time join tokens.
master *ARGS:
	DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-}:$(brew --prefix)/lib" \
		dotnet run --project apps/master -- {{ARGS}}

# Journal persists in ./data. Joining is token-only, so a server without a
# master registration can admit no one. --admin-name <player> grants that
# player /help /tp /ban /ban-list /unban in chat.
# e.g. just server -- --master-url http://127.0.0.1:5200 --advertise-host 127.0.0.1
#
# Dedicated multiplayer server (headless). Requires a running master.
server *ARGS:
	cargo run -p digblock-server --bin digblock-server -- {{ARGS}}

# Picks a server from the master's list (the first, or `--server
# NAME_FILTER`), mints one token per bot, then joins.
#
# Headless bot client(s): `just bot -- --bots 4`.
bot *ARGS:
	cargo run -p digblock-server --bin bot -- {{ARGS}}

# Containerized master (real QUIC probe, same image as production and the
# e2e tests) + native server + a client opening at the main menu (pick
# Multiplayer, join "Local World" through the token flow); closing the
# window (or Ctrl-C) stops all three. The server
# advertises 127.0.0.1 (what host clients dial) and gives the master
# host.docker.internal as its probe address — the only stable UDP path from
# the container to the host (the LAN-IP hairpin breaks QUIC address
# validation under Docker's NAT).
# Extra clients: `just run -- --master-url http://127.0.0.1:5200` (add
# --join --name <name> to skip the menu for scripted flows; display
# names are mandatory, 3-10 chars of a-z A-Z 0-9 - _). In-game: T or
# Enter opens chat; --mute silences the audio stack.
#
# One-command multiplayer test: master + server + client, torn down together.
play *ARGS:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p digblock-server --bin digblock-server
	cargo build -p digblock-desktop
	docker build -q -t digblock-master apps/master >/dev/null
	server=""
	docker rm -f digblock-play-master >/dev/null 2>&1 || true
	docker run -d --rm --name digblock-play-master \
		-p 127.0.0.1:5200:5200 digblock-master >/dev/null
	trap 'kill $server 2>/dev/null || true; docker rm -f digblock-play-master >/dev/null 2>&1 || true' EXIT
	until curl -sf http://127.0.0.1:5200/api/servers >/dev/null 2>&1; do sleep 0.2; done
	./target/debug/digblock-server --master-url http://127.0.0.1:5200 \
		--advertise-host 127.0.0.1 --probe-host host.docker.internal \
		--server-name "Local World" --admin-name "${DIGBLOCK_ADMIN_NAME:-admin}" &
	server=$!
	until curl -s http://127.0.0.1:5200/api/servers | grep -q '"id"'; do sleep 0.2; done
	./target/debug/digblock --master-url http://127.0.0.1:5200 {{ARGS}}

# `just play` with the game server containerized too (compose), optionally
# behind netem — `just play-docker wifi` plays under simulated wifi loss.
# Same probe-host split, so the master's QUIC probe stays real: the server
# advertises 127.0.0.1 for the host client while the master probes
# host.docker.internal, which loops through the published UDP port back to
# the server container. Host<->container UDP works fine; if the client
# times out, check `docker logs docker-server-1` — a server that died at
# boot looks identical to a network problem from the client side.
#
# Desktop client against the dockerized stack, behind netem: clean | wifi | cellular.
play-docker PROFILE="clean" *ARGS:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p digblock-desktop
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
	ADVERTISE_HOST=127.0.0.1 PROBE_HOST=host.docker.internal \
		docker compose -f docker/compose.yml up --build -d master server
	trap 'docker compose -f docker/compose.yml down' EXIT
	./target/debug/digblock --master-url http://127.0.0.1:5200 {{ARGS}}

# The `trap ... down` is not a nicety, and it is why this recipe carries a
# shebang for one command: `up` in the FOREGROUND leaves its containers
# behind on Ctrl-C, which is how a run gets abandoned and then sits there
# for weeks. `play` and `play-docker` have always cleaned up after
# themselves; these two were the pair that didn't.
#
# Dockerized server behind a tc netem profile: clean | wifi | cellular.
compose PROFILE="clean":
	#!/usr/bin/env bash
	set -euo pipefail
	trap 'docker compose -f docker/compose.yml down --remove-orphans' EXIT
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml up --build server

# `--remove-orphans` matters more here than next door: the bots service is
# behind a compose PROFILE, so a later `just compose` without it does not
# consider the bots container part of the project and leaves it standing.
# That is exactly how a `docker-bots-1` came to sit Exited for nine days.
# The trap runs on the non-zero exit too — a failed soak is the run you
# most want cleaned up, since you are about to start another.
#
# `just compose` plus a bot soak that exits non-zero on missed snapshots.
compose-bots PROFILE="clean":
	#!/usr/bin/env bash
	set -euo pipefail
	trap 'docker compose -f docker/compose.yml --profile bots down --remove-orphans' EXIT
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml --profile bots up --build --abort-on-container-exit

# localhost is a secure context, so WebGPU and WebTransport both work over
# plain http. Needs wasm-bindgen-cli at the exact version in Cargo.lock:
# cargo install wasm-bindgen-cli.
# Query params mirror desktop flags: ?seed= ?tod= ?draw= ?ssr=0 ?overlay
# ?name= (validated: 3-10 chars of a-z A-Z 0-9 - _) ?mute.
#
# Browser client: release wasm build + bindgen + a static server on :8080.
web:
	#!/usr/bin/env bash
	set -euo pipefail
	just check-port-8080
	just build-web
	echo "→ http://localhost:8080"
	python3 -c 'import http.server, functools, mimetypes; \
		mimetypes.add_type("application/wasm", ".wasm"); \
		http.server.test(HandlerClass=functools.partial(http.server.SimpleHTTPRequestHandler, directory="target/web"), port=8080, bind="127.0.0.1")'

# `just web` already defaults to the production master; this differs only
# by probing it FIRST, so a deploy outage fails loudly here instead of
# looking like a broken menu three clicks in. QUERY replaces the default
# params (`just web-live '?draw=32&overlay'`), which are the frame-pacing
# diagnostic pair: `overlay` for the live busy/out split, `spikelog` for
# per-frame attribution in the console.
#
# Browser client against the live master.digblock.net, diagnostics on.
web-live QUERY="?overlay&spikelog":
	#!/usr/bin/env bash
	set -euo pipefail
	just check-port-8080
	curl -sf --max-time 10 https://master.digblock.net/api/servers >/dev/null \
		|| { echo "master.digblock.net unreachable — not starting" >&2; exit 1; }
	just build-web
	echo "→ http://localhost:8080/{{QUERY}}   (master: https://master.digblock.net)"
	python3 -c 'import http.server, functools, mimetypes; \
		mimetypes.add_type("application/wasm", ".wasm"); \
		http.server.test(HandlerClass=functools.partial(http.server.SimpleHTTPRequestHandler, directory="target/web"), port=8080, bind="127.0.0.1")'

# Same topology as `just play`. Open the printed URL in Chrome, pick
# Multiplayer, type a name (3-10 chars), join "Local World"; append `&join`
# to skip the menu (scripted flows), `&name=X` for a display name.
#
# Browser multiplayer test: containerized master + native server + the bundle on :8080.
play-web:
	#!/usr/bin/env bash
	set -euo pipefail
	just check-port-8080
	just build-web
	cargo build -p digblock-server --bin digblock-server
	docker build -q -t digblock-master apps/master >/dev/null
	server=""
	docker rm -f digblock-play-master >/dev/null 2>&1 || true
	docker run -d --rm --name digblock-play-master \
		-p 127.0.0.1:5200:5200 digblock-master >/dev/null
	trap 'kill $server 2>/dev/null || true; docker rm -f digblock-play-master >/dev/null 2>&1 || true' EXIT
	until curl -sf http://127.0.0.1:5200/api/servers >/dev/null 2>&1; do sleep 0.2; done
	./target/debug/digblock-server --master-url http://127.0.0.1:5200 \
		--advertise-host 127.0.0.1 --probe-host host.docker.internal \
		--server-name "Local World" --admin-name "${DIGBLOCK_ADMIN_NAME:-admin}" &
	server=$!
	until curl -s http://127.0.0.1:5200/api/servers | grep -q '"id"'; do sleep 0.2; done
	echo "→ http://localhost:8080/?master=http://127.0.0.1:5200"
	python3 -c 'import http.server, functools, mimetypes; \
		mimetypes.add_type("application/wasm", ".wasm"); \
		http.server.test(HandlerClass=functools.partial(http.server.SimpleHTTPRequestHandler, directory="target/web"), port=8080, bind="127.0.0.1")'

# Fail fast (before the wasm build) if a previous web/play-web is still
# holding :8080 — the python bind error at the end is much less friendly.
[private]
check-port-8080:
	#!/usr/bin/env bash
	set -euo pipefail
	pids=$(lsof -ti tcp:8080 -sTCP:LISTEN || true)
	if [ -n "$pids" ]; then
		echo "error: port 8080 is already in use:" >&2
		ps -o pid=,command= -p $pids >&2
		echo "kill it first: kill $pids" >&2
		exit 1
	fi

# The glue JS and the wasm are ONE artifact — wasm-bindgen mangles import
# names with a per-build hash — so a browser that pairs a fresh wasm with a
# stale glue dies with `LinkError: ... requires a callable`. Cache headers
# alone cannot prevent that: `_headers` reaches the Pages origin, but the
# CDN in front of a custom domain rewrites Cache-Control for .js (a default
# static extension) and not for .wasm, which re-opens the exact skew.
#
# So the pair is content-addressed instead. Both files carry the same build
# stamp in their NAME, and only index.html — served must-revalidate, and not
# a cacheable extension anywhere — knows what that stamp is. A stale index
# can then only ask for a pair that no longer exists (a clean fetch error,
# self-healing on reload), never for half of a live one.
#
# Build + bindgen the web bundle into target/web (no server).
build-web:
	#!/usr/bin/env bash
	set -euo pipefail
	want=$(grep -A2 'name = "wasm-bindgen"' Cargo.lock | grep version | head -1 | cut -d'"' -f2)
	have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
	if [ "$want" != "$have" ]; then
		echo "wasm-bindgen-cli $want required (found: ${have:-none})."
		echo "  cargo install wasm-bindgen-cli --version $want"
		exit 1
	fi
	cargo build -p digblock-web -p digblock-web-worker \
		--target wasm32-unknown-unknown --release
	wasm=target/wasm32-unknown-unknown/release/digblock_web.wasm
	worker=target/wasm32-unknown-unknown/release/digblock_web_worker.wasm
	# The bindgen version is part of the stamp: it decides the mangled
	# import names too, so the same wasm through a new CLI is a new pair.
	if command -v sha256sum >/dev/null 2>&1; then sha=sha256sum; else sha="shasum -a 256"; fi
	stamp=$( { cat "$wasm"; echo "$want"; } | $sha | cut -c1-12)
	# The chunk-job worker (apps/web-worker) is a SECOND, independent pair:
	# its own module, its own glue, its own stamp from its own bytes. Two
	# pairs rather than one shared stamp, because a shared one would rename
	# both modules whenever either changed and force clients to re-download
	# a worker that hadn't moved. The invariant is unchanged and simply
	# applied twice — each pair is immutable by name, and only index.html
	# knows either stamp.
	wstamp=$( { cat "$worker"; echo "$want"; } | $sha | cut -c1-12)
	# Wipe the dir, or every local build leaves its predecessor behind and
	# `pages deploy` uploads the lot.
	rm -rf target/web/pkg
	wasm-bindgen --target web --no-typescript --out-dir target/web/pkg \
		--out-name "digblock_web_$stamp" "$wasm"
	wasm-bindgen --target web --no-typescript --out-dir target/web/pkg \
		--out-name "digblock_worker_$wstamp" "$worker"
	# Rewrites every reference at once: within each pair the `.js` and the
	# `_bg.wasm` share the prefix wasm-bindgen just stamped. The two
	# patterns cannot collide — `digblock_worker` does not contain
	# `digblock_web` — but the worker goes first regardless, so a future
	# rename can only break the build rather than mis-stamp a live pair.
	sed -e "s|digblock_worker|digblock_worker_$wstamp|g" \
		-e "s|digblock_web|digblock_web_$stamp|g" \
		apps/web/index.html > target/web/index.html
	cp apps/web/_headers target/web/

# Compiles every shader with the front end that SHIPS them, by handing
# them to a headless Chrome. naga (what `cargo test` runs) is the
# permissive end and Tint is not, so a shader can pass every native gate
# and still invalidate a whole pipeline in the browser — that has shipped
# twice, most recently `highlight.wgsl` mixing `*` and `^` unparenthesized,
# which cost the block outline and the dig crack on web. Needs Chrome
# installed; `DIG_CHROME=/path/to/chrome` if it lives somewhere unusual.
#
# Compile every shader in real Chrome — the gate naga cannot be.
check-shaders:
	cargo run -q -p shadercheck

# Needs Chrome for the shader half, but still no wasm-bindgen-cli.
#
# Fast "is the web client still green" gate — wasm32 clippy plus check-shaders.
check-web:
	cargo clippy -p digblock-web -p digblock-web-worker -p digblock-render \
		-p digblock-platform --target wasm32-unknown-unknown -- -D warnings
	just check-shaders

# The `cargo fmt` of the prose side, so editing a sentence touches a line
# instead of reflowing a whole paragraph into the diff. Needs `cargo install
# dprint --locked`. The markdown PLUGIN is version-pinned in dprint.json for
# the reason the Docker tags are pinned — the plugin decides the wrapping,
# and one that drifts rewrites every doc at once into an unreviewable diff.
#
# Re-wrap every Markdown file to the width dprint.json sets.
fmt-md:
	dprint fmt

# The check half of `fmt-md`, for the gate. Reports, never writes.
check-md:
	dprint check

# Needs Docker: the server e2e tests boot the apps/master container.
#
# The full gate — fmt, Markdown wrap, clippy -D warnings, the whole test suite.
check:
	cargo fmt --all --check
	just check-md
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

# Fast "is iOS still green" gate — no Xcode required.
check-ios:
	cargo check -p digblock-ios --target aarch64-apple-ios

# Requires full Xcode (not just Command Line Tools).
#
# Build, install and launch the iOS client on the Simulator.
ios-sim:
	xcodebuild -project apps/ios/xcode/DigBlock.xcodeproj -scheme DigBlock \
		-destination 'platform=iOS Simulator,name=iPhone 16' \
		-configuration Debug -derivedDataPath build build CODE_SIGNING_ALLOWED=NO
	xcrun simctl boot "iPhone 16" || true
	xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/DigBlock.app
	xcrun simctl launch booted ca.valacware.digblock

# Signed device build (requires full Xcode). Installing and launching stay Xcode's job.
ios-device:
	xcodebuild -project apps/ios/xcode/DigBlock.xcodeproj -scheme DigBlock \
		-destination 'generic/platform=iOS' -configuration Debug build -allowProvisioningUpdates

# Release-speed build with debug symbols, for Instruments/profilers.
profile *ARGS:
	cargo run --profile profiling -p digblock-desktop -- {{ARGS}}

# Remove every Docker artifact this project creates, and nothing else.
#
# Scoped ON PURPOSE — `docker system prune` would take other projects'
# containers and volumes with it, and a volume is where a database keeps
# the only copy of its data. Everything named here is rebuilt on demand:
# the e2e suite runs its own `docker build`, and compose builds with
# `--build`, so the cost of being wrong is a slower next run.
#
# What this deliberately does NOT touch is the build CACHE, because that
# is shared across every project on the machine. Cap it once at the
# daemon instead of pruning it by hand — in ~/.docker/daemon.json,
# `builder.gc.policy[].maxUsedSpace` is the ceiling. `defaultKeepStorage`
# and `reservedSpace` read like caps and are not: they are the floor GC
# refuses to prune BELOW, so setting only those lets the cache grow
# without limit (measured here at 40GB against a 20GB "keep").
#
# Remove this project's containers and images; cache and other projects untouched.
docker-clean:
	#!/usr/bin/env bash
	set -euo pipefail
	docker compose -f docker/compose.yml --profile bots down --remove-orphans 2>/dev/null || true
	docker rm -f digblock-play-master 2>/dev/null || true
	for img in digblock-master digblock-master-e2e digblock-server docker-master docker-server docker-bots; do
		docker rmi -f "$img" 2>/dev/null || true
	done
	echo "digblock docker artifacts removed; build cache left alone (see comment)"
	docker system df
