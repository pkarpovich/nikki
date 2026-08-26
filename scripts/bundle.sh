#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
if [ -z "$version" ]; then
	echo "bundle: no version in Cargo.toml" >&2
	exit 1
fi

cargo build --release

app="target/nikki.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/release/nikki "$app/Contents/MacOS/nikki"
sed "s/__VERSION__/$version/g" Info.plist.template >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"

echo "$app"
