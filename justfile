# DigBlock-rs build commands

run *ARGS:
	cargo run -p digblock-desktop -- {{ARGS}}

# Deterministic perf run (release build — dev numbers aren't representative).
bench *ARGS:
	cargo run --release -p digblock-desktop -- --bench {{ARGS}}

# The .NET master server (server list + one-time join tokens), run natively
# for iterating on its code. The QUIC reachability probe is mandatory, so
# this needs `brew install libmsquic`; without it, use the container
# (`just play` builds and runs it automatically).
master *ARGS:
	DYLD_FALLBACK_LIBRARY_PATH="${DYLD_FALLBACK_LIBRARY_PATH:-}:$(brew --prefix)/lib" \
		dotnet run --project apps/master -- {{ARGS}}

# Dedicated multiplayer server (headless). Journal persists in ./data.
# Requires a running master: joining is token-only, so a server without a
# master registration can admit no one. --admin-name <player> grants that
# player /help /tp /ban /ban-list /unban in chat.
# e.g. just server -- --master-url http://127.0.0.1:5200 --advertise-host 127.0.0.1
server *ARGS:
	cargo run -p digblock-server --bin digblock-server -- {{ARGS}}

# Headless bot client(s): pick a server from the master's list (first, or
# --server NAME_FILTER), mint one token per bot, join.
bot *ARGS:
	cargo run -p digblock-server --bin bot -- {{ARGS}}

# One-command multiplayer test: containerized master (real QUIC probe, same
# image as production and the e2e tests) + native server + a client opening
# at the main menu (pick Multiplayer, join "Local World" through the token
# flow); closing the window (or Ctrl-C) stops all three. The server
# advertises 127.0.0.1 (what host clients dial) and gives the master
# host.docker.internal as its probe address — the only stable UDP path from
# the container to the host (the LAN-IP hairpin breaks QUIC address
# validation under Docker's NAT).
# Extra clients: `just run -- --master-url http://127.0.0.1:5200` (add
# --join --name <name> to skip the menu for scripted flows; display
# names are mandatory, 3-10 chars of a-z A-Z 0-9 - _). In-game: T or
# Enter opens chat; --mute silences the audio stack.
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
play-docker PROFILE="clean" *ARGS:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p digblock-desktop
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
	ADVERTISE_HOST=127.0.0.1 PROBE_HOST=host.docker.internal \
		docker compose -f docker/compose.yml up --build -d master server
	trap 'docker compose -f docker/compose.yml down' EXIT
	./target/debug/digblock --master-url http://127.0.0.1:5200 {{ARGS}}

# Dockerized server behind a tc netem profile: clean | wifi | cellular.
compose PROFILE="clean":
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml up --build server

# Same, plus a 4-bot soak that exits non-zero on missed snapshots.
compose-bots PROFILE="clean":
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml --profile bots up --build --abort-on-container-exit

# Browser client: release wasm build + bindgen + static server at
# http://localhost:8080 (localhost is a secure context — WebGPU and
# WebTransport both work over plain http). Needs wasm-bindgen-cli at the
# exact version in Cargo.lock: cargo install wasm-bindgen-cli.
# Query params mirror desktop flags: ?seed= ?tod= ?draw= ?ssr=0 ?shadow=
# ?overlay ?name= (validated: 3-10 chars of a-z A-Z 0-9 - _) ?mute.
web:
	#!/usr/bin/env bash
	set -euo pipefail
	just check-port-8080
	just build-web
	echo "→ http://localhost:8080"
	python3 -c 'import http.server, functools, mimetypes; \
		mimetypes.add_type("application/wasm", ".wasm"); \
		http.server.test(HandlerClass=functools.partial(http.server.SimpleHTTPRequestHandler, directory="target/web"), port=8080, bind="127.0.0.1")'

# Browser multiplayer test: containerized master + native server (same
# topology as `just play`) + the web bundle served on :8080. Open the
# printed URL in Chrome, pick Multiplayer, type a name (3-10 chars),
# join "Local World"; append `&join` to skip the menu (scripted flows),
# `&name=X` for a display name.
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
	cargo build -p digblock-web --target wasm32-unknown-unknown --release
	wasm-bindgen --target web --no-typescript --out-dir target/web/pkg \
		target/wasm32-unknown-unknown/release/digblock_web.wasm
	cp apps/web/index.html target/web/

# Fast "is the web client still green" gate — no wasm-bindgen-cli needed.
check-web:
	cargo clippy -p digblock-web -p digblock-render -p digblock-platform \
		--target wasm32-unknown-unknown -- -D warnings

check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

# Fast "is iOS still green" gate — no Xcode required.
check-ios:
	cargo check -p digblock-ios --target aarch64-apple-ios

# Requires full Xcode (not just Command Line Tools).
ios-sim:
	xcodebuild -project apps/ios/xcode/DigBlock.xcodeproj -scheme DigBlock \
		-destination 'platform=iOS Simulator,name=iPhone 16' \
		-configuration Debug -derivedDataPath build build CODE_SIGNING_ALLOWED=NO
	xcrun simctl boot "iPhone 16" || true
	xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/DigBlock.app
	xcrun simctl launch booted ca.valacware.digblock

ios-device:
	xcodebuild -project apps/ios/xcode/DigBlock.xcodeproj -scheme DigBlock \
		-destination 'generic/platform=iOS' -configuration Debug build -allowProvisioningUpdates

# Release-speed build with debug symbols, for Instruments/profilers.
profile *ARGS:
	cargo run --profile profiling -p digblock-desktop -- {{ARGS}}
