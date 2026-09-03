#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
manifest="$repo_root/codex-rs/Cargo.toml"
lockfile="$repo_root/codex-rs/Cargo.lock"
binary="$repo_root/codex-rs/target/debug/codex"

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

if ! git -C "$repo_root" diff --quiet -- "$manifest" "$lockfile" ||
  ! git -C "$repo_root" diff --cached --quiet -- "$manifest" "$lockfile"; then
  fail "Cargo.toml or Cargo.lock has uncommitted changes"
fi

official_codex=${CODEX_OFFICIAL_BIN:-}
if [ -z "$official_codex" ]; then
  for candidate in /opt/homebrew/bin/codex /usr/local/bin/codex; do
    if [ -x "$candidate" ]; then
      official_codex=$candidate
      break
    fi
  done
fi
if [ -z "$official_codex" ]; then
  official_codex=$(command -v codex || true)
fi
if [ -z "$official_codex" ] || [ ! -x "$official_codex" ]; then
  fail "official Codex CLI not found; set CODEX_OFFICIAL_BIN=/path/to/codex"
fi

official_output=$("$official_codex" --version)
official_version=$(printf '%s\n' "$official_output" | sed -n 's/^codex-cli \([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p')
if [ -z "$official_version" ]; then
  fail "could not parse official version from: $official_output"
fi
if [ "$official_version" = "0.0.0" ]; then
  fail "resolved Codex is a source build, not an official release: $official_codex"
fi

short_sha=$(git -C "$repo_root" rev-parse --short=10 HEAD)
build_version="$official_version+debug.$short_sha"

if [ "${1:-}" = "--print-version" ]; then
  printf '%s\n' "$build_version"
  exit 0
fi
if [ "$#" -ne 0 ]; then
  fail "usage: $0 [--print-version]"
fi

if [ "$(git -C "$repo_root" branch --show-current)" != "main" ]; then
  fail "run this script from the primary main checkout; worktrees are edit-only"
fi

backup_dir=$(mktemp -d "${TMPDIR:-/tmp}/codex-debug-build.XXXXXX")
cp "$manifest" "$backup_dir/Cargo.toml"
cp "$lockfile" "$backup_dir/Cargo.lock"

restore_sources() {
  cp "$backup_dir/Cargo.toml" "$manifest"
  cp "$backup_dir/Cargo.lock" "$lockfile"
  rm -f "$backup_dir/Cargo.toml" "$backup_dir/Cargo.lock"
  rmdir "$backup_dir"
}
trap restore_sources EXIT
trap 'exit 1' HUP INT TERM

python3 - "$manifest" "$build_version" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
version = sys.argv[2]
text = path.read_text(encoding="utf-8")
updated, count = re.subn(
    r'(\[workspace\.package\]\nversion\s*=\s*")[^"]+("\n)',
    rf'\g<1>{version}\g<2>',
    text,
    count=1,
)
if count != 1:
    raise SystemExit("workspace package version was not found exactly once")
path.write_text(updated, encoding="utf-8")
PY

printf 'Building Codex debug %s from %s\n' "$build_version" "$short_sha"
(cd "$repo_root/codex-rs" && cargo build -p codex-cli --bin codex)

if [ "$(uname -s)" = "Darwin" ]; then
  if ! command -v codesign >/dev/null 2>&1; then
    fail "codesign is required to make the debug binary executable on macOS"
  fi
  codesign --force --sign - "$binary"
  codesign --verify --strict "$binary"
fi

actual_version=$("$binary" --version)
expected_version="codex-cli $build_version"
if [ "$actual_version" != "$expected_version" ]; then
  fail "built binary reported '$actual_version'; expected '$expected_version'"
fi

printf 'Built %s\n' "$actual_version"
printf 'Binary: %s\n' "$binary"
