#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-target}"
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"wakuwaku","version":"\([^"]*\)".*/\1/p')"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
package="wakuwaku-${version}-${target_triple}"
archive="$target_dir/release/$package.tar.gz"
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT

cargo build --locked --release --package wakuwaku --bin wakuwaku --package wakuwaku-daemon --bin wakuwaku-daemon

package_dir="$staging/$package"
install -Dm755 "$target_dir/release/wakuwaku" "$package_dir/bin/wakuwaku"
install -Dm755 "$target_dir/release/wakuwaku-daemon" "$package_dir/bin/wakuwaku-daemon"
install -Dm644 resources/linux/dev.bingzi.wakuwaku.desktop \
  "$package_dir/share/applications/dev.bingzi.wakuwaku.desktop"
install -Dm644 website/public/app-icon.png \
  "$package_dir/share/icons/hicolor/256x256/apps/dev.bingzi.wakuwaku.png"
install -Dm644 LICENSE "$package_dir/share/licenses/wakuwaku/LICENSE"

mkdir -p "$(dirname "$archive")"
tar -C "$staging" -czf "$archive" "$package"
printf 'Created %s\n' "$archive"
