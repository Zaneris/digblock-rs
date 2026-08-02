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
# master registration can admit no one.
# e.g. just server -- --master-url http://127.0.0.1:5200 --advertise-host 127.0.0.1
server *ARGS:
	cargo run -p digblock-server --bin digblock-server -- {{ARGS}}

# Headless bot client(s): pick a server from the master's list (first, or
# --server NAME_FILTER), mint one token per bot, join.
bot *ARGS:
	cargo run -p digblock-server --bin bot -- {{ARGS}}

# One-command multiplayer test: containerized master (real QUIC probe, same
# image as production and the e2e tests) + native server + a client that
# auto-joins through the token flow; closing the window (or Ctrl-C) stops
# all three. The server advertises 127.0.0.1 (what host clients dial) and
# gives the master host.docker.internal as its probe address — the only
# stable UDP path from the container to the host (the LAN-IP hairpin breaks
# QUIC address validation under Docker's NAT).
# Extra clients: `just run -- --master-url http://127.0.0.1:5200 --join`
# (or without --join for the menu).
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
		--server-name "Local World" &
	server=$!
	until curl -s http://127.0.0.1:5200/api/servers | grep -q '"id"'; do sleep 0.2; done
	./target/debug/digblock --master-url http://127.0.0.1:5200 --join {{ARGS}}

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
	./target/debug/digblock --master-url http://127.0.0.1:5200 --join {{ARGS}}

# Dockerized server behind a tc netem profile: clean | wifi | cellular.
compose PROFILE="clean":
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml up --build server

# Same, plus a 4-bot soak that exits non-zero on missed snapshots.
compose-bots PROFILE="clean":
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml --profile bots up --build --abort-on-container-exit

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
