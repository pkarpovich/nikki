#!/usr/bin/env bash
set -euo pipefail

IDENTIFIER="space.pkarpovich.nikki"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "acceptance: the daemon against the stub server"
cargo test --test stub_server -- --nocapture

echo "acceptance: the embedded Info.plist"
cargo build --release
binary="target/release/nikki"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

plist="$scratch/Info.plist"
otool -P "$binary" | tail -n +3 >"$plist"

if ! plutil -lint "$plist" >/dev/null 2>&1; then
	echo "acceptance: $binary carries no readable __TEXT,__info_plist section" >&2
	exit 1
fi

identifier="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist" 2>/dev/null || echo missing)"
if [ "$identifier" != "$IDENTIFIER" ]; then
	echo "acceptance: CFBundleIdentifier is $identifier, so every TCC grant made against $IDENTIFIER is lost" >&2
	exit 1
fi

usage="$(/usr/libexec/PlistBuddy -c 'Print :NSAppleEventsUsageDescription' "$plist" 2>/dev/null || echo '')"
if [ -z "$usage" ]; then
	echo "acceptance: NSAppleEventsUsageDescription is empty, so macOS never offers the automation prompt" >&2
	exit 1
fi

echo "acceptance: --check-config"
cat >"$scratch/config.toml" <<'TOML'
service_url = "http://alpha:8080"
device = "mbp-21"

[browser]
profile = "MBP_21"
TOML

if ! NIKKI_CONFIG="$scratch/config.toml" NIKKI_STATE_DIR="$scratch/state" "$binary" --check-config >/dev/null; then
	echo "acceptance: --check-config rejected a valid configuration" >&2
	exit 1
fi

printf 'device = "mbp-21"\n' >"$scratch/broken.toml"
if NIKKI_CONFIG="$scratch/broken.toml" NIKKI_STATE_DIR="$scratch/state" "$binary" --check-config >/dev/null 2>&1; then
	echo "acceptance: --check-config accepted a configuration with no service_url" >&2
	exit 1
fi

echo "acceptance: every check passed, and $binary is the binary they ran against"
