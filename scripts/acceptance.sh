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

echo "acceptance: every check passed, and $app is the bundle they ran against"
