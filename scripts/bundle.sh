#!/usr/bin/env bash
set -euo pipefail

IDENTIFIER="dev.pkarpovich.nikki"
APP="Nikki.app"

if [ $# -lt 2 ]; then
	echo "usage: ${0##*/} <binary> <out-dir> [identity]" >&2
	echo "assembles $APP around a built nikki and signs it when an identity is given" >&2
	exit 2
fi

binary=$1
out_dir=$2
identity=${3:-}
root=$(cd "$(dirname "$0")/.." && pwd)

if [ ! -x "$binary" ]; then
	echo "$binary is not an executable" >&2
	exit 1
fi

version=$(grep -m1 '^version' "$root/Cargo.toml" | cut -d'"' -f2)
if [ -z "$version" ]; then
	echo "no version in Cargo.toml" >&2
	exit 1
fi

app="$out_dir/$APP"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"

sed "s/__VERSION__/$version/g" "$root/Info.plist.template" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
cp "$binary" "$app/Contents/MacOS/nikki"

# The bundle is what makes a macOS permission durable: TCC identifies a bundle by
# its identifier at a path that does not move, and a loose binary by its path
# alone - which Homebrew changes on every version.
if [ -n "$identity" ]; then
	codesign \
		--force \
		--timestamp \
		--options runtime \
		--identifier "$IDENTIFIER" \
		--sign "$identity" \
		"$app"
	codesign --verify --strict --deep --verbose=2 "$app"
	codesign --display "$app" 2>&1 | grep '^Identifier='
fi

echo "$app"
