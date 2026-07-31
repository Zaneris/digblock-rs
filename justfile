# DigBlock-rs build commands

run *ARGS:
	cargo run -p digblock-desktop -- {{ARGS}}

# Deterministic perf run (release build — dev numbers aren't representative).
bench *ARGS:
	cargo run --release -p digblock-desktop -- --bench {{ARGS}}

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
