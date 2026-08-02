# DigBlock-rs build commands

run *ARGS:
	cargo run -p digblock-desktop -- {{ARGS}}

# Deterministic perf run (release build — dev numbers aren't representative).
bench *ARGS:
	cargo run --release -p digblock-desktop -- --bench {{ARGS}}

# Dedicated multiplayer server (headless). Journal persists in ./data.
server *ARGS:
	cargo run -p digblock-server --bin digblock-server -- {{ARGS}}

# Headless bot client(s) against a running server.
bot *ARGS:
	cargo run -p digblock-server --bin bot -- {{ARGS}}

# One-command multiplayer test: local server + connected client; closing the
# window (or Ctrl-C) stops both. Compose can't do this on macOS — the client
# needs a real display/GPU and Docker Desktop's UDP proxy drops QUIC anyway
# (see docker/compose.yml). 127.0.0.1, not localhost: macOS resolves localhost
# to ::1 and the server binds IPv4 only, so localhost times out.
# Extra clients: `just run -- --connect https://127.0.0.1:4433`.
play *ARGS:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p digblock-server --bin digblock-server
	cargo build -p digblock-desktop
	./target/debug/digblock-server &
	server=$!
	trap 'kill "$server" 2>/dev/null || true' EXIT
	until lsof -nP -iUDP:4433 >/dev/null 2>&1; do sleep 0.2; done
	./target/debug/digblock --connect https://127.0.0.1:4433 {{ARGS}}

# `just play` with the server containerized instead (compose), optionally
# behind netem — `just play-docker wifi` plays under simulated wifi loss.
# Needs Docker Desktop's "Use kernel networking for UDP" (Settings >
# Resources > Network); the default user-space UDP proxy drops QUIC.
play-docker PROFILE="clean" *ARGS:
	#!/usr/bin/env bash
	set -euo pipefail
	cargo build -p digblock-desktop
	NETEM="{{ if PROFILE == "wifi" { "delay 30ms loss 1%" } else if PROFILE == "cellular" { "delay 80ms 20ms distribution normal loss 3%" } else { "" } }}" \
		docker compose -f docker/compose.yml up --build -d server
	trap 'docker compose -f docker/compose.yml down' EXIT
	./target/debug/digblock --connect https://127.0.0.1:4433 {{ARGS}}

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
