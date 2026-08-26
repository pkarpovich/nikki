#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "acceptance: the daemon against the stub server"
cargo test --test stub_server -- --nocapture

echo "acceptance: the app bundle"
app="$(scripts/bundle.sh | tail -1)"
plist="$app/Contents/Info.plist"

if [ ! -x "$app/Contents/MacOS/nikki" ]; then
	echo "acceptance: $app carries no executable" >&2
	exit 1
fi

ui_element="$(/usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$plist" 2>/dev/null || echo missing)"
if [ "$ui_element" != "true" ]; then
	echo "acceptance: LSUIElement is $ui_element, so the daemon would take a dock icon and a menu bar" >&2
	exit 1
fi

usage="$(/usr/libexec/PlistBuddy -c 'Print :NSAppleEventsUsageDescription' "$plist" 2>/dev/null || echo '')"
if [ -z "$usage" ]; then
	echo "acceptance: NSAppleEventsUsageDescription is empty, so macOS never offers the automation prompt" >&2
	exit 1
fi

echo "acceptance: --check-config"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
cat >"$scratch/config.toml" <<'TOML'
service_url = "http://alpha:8080"
device = "mbp-21"

[browser]
profile = "MBP_21"
TOML

if ! NIKKI_CONFIG="$scratch/config.toml" NIKKI_STATE_DIR="$scratch/state" "$app/Contents/MacOS/nikki" --check-config >/dev/null; then
	echo "acceptance: --check-config rejected a valid configuration" >&2
	exit 1
fi

printf 'device = "mbp-21"\n' >"$scratch/broken.toml"
if NIKKI_CONFIG="$scratch/broken.toml" NIKKI_STATE_DIR="$scratch/state" "$app/Contents/MacOS/nikki" --check-config >/dev/null 2>&1; then
	echo "acceptance: --check-config accepted a configuration with no service_url" >&2
	exit 1
fi

echo "acceptance: every check passed, and $app is the bundle they ran against"
