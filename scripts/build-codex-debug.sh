#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
manifest="$repo_root/codex-rs/Cargo.toml"
lockfile="$repo_root/codex-rs/Cargo.lock"
target_dir=${CARGO_TARGET_DIR:-"$repo_root/codex-rs/target"}
case "$target_dir" in
  /*) ;;
  *) target_dir="$repo_root/codex-rs/$target_dir" ;;
esac
binary="$target_dir/debug/codex"
host_binary="$target_dir/debug/codex-code-mode-host"
install_dir=${CODEX_DEBUG_INSTALL_DIR:-"$HOME/.local/lib/codex"}
bin_dir=${CODEX_DEBUG_BIN_DIR:-"$HOME/.local/bin"}
installed_binary="$install_dir/codex-debug"
installed_host_binary="$install_dir/codex-code-mode-host"

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
  rm -f "$backup_dir/Cargo.toml" "$backup_dir/Cargo.lock" "$backup_dir/v8-paths"
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

cargo_target=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$cargo_target" ]; then
  fail "could not determine the Rust host target"
fi

v8_paths="$backup_dir/v8-paths"
CODEX_REPO_ROOT="$repo_root" PYTHONPATH="$repo_root/scripts" python3 - "$cargo_target" "$v8_paths" <<'PY'
import sys
from pathlib import Path

from codex_package.targets import TARGET_SPECS
from codex_package.v8 import resolve_codex_v8_cargo_env

target = sys.argv[1]
output = Path(sys.argv[2])
try:
    spec = TARGET_SPECS[target]
except KeyError as exc:
    raise SystemExit(f"unsupported Rust host target: {target}") from exc

env = resolve_codex_v8_cargo_env(spec)
output.write_text(
    f"{env.get('RUSTY_V8_ARCHIVE', '')}\n"
    f"{env.get('RUSTY_V8_SRC_BINDING_PATH', '')}\n",
    encoding="utf-8",
)
PY
v8_archive=$(sed -n '1p' "$v8_paths")
v8_binding=$(sed -n '2p' "$v8_paths")

printf 'Building Codex debug %s from %s\n' "$build_version" "$short_sha"
if [ -n "$v8_archive" ] && [ -n "$v8_binding" ]; then
  (
    cd "$repo_root/codex-rs"
    RUSTY_V8_ARCHIVE="$v8_archive" \
      RUSTY_V8_SRC_BINDING_PATH="$v8_binding" \
      cargo build \
        -p codex-cli --bin codex \
        -p codex-code-mode-host --bin codex-code-mode-host
  )
else
  (
    cd "$repo_root/codex-rs"
    cargo build \
      -p codex-cli --bin codex \
      -p codex-code-mode-host --bin codex-code-mode-host
  )
fi

if [ "$(uname -s)" = "Darwin" ]; then
  if ! command -v codesign >/dev/null 2>&1; then
    fail "codesign is required to make the debug binaries executable on macOS"
  fi
  for artifact in "$binary" "$host_binary"; do
    codesign --force --sign - "$artifact"
    codesign --verify --strict "$artifact"
  done
fi

actual_version=$("$binary" --version)
expected_version="codex-cli $build_version"
if [ "$actual_version" != "$expected_version" ]; then
  fail "built binary reported '$actual_version'; expected '$expected_version'"
fi
"$host_binary" --help >/dev/null

mkdir -p "$install_dir" "$bin_dir"
for link in "$bin_dir/codex-debug" "$bin_dir/codex-code-mode-host"; do
  if [ -e "$link" ] && [ ! -L "$link" ]; then
    fail "refusing to replace non-symlink executable: $link"
  fi
done

install_artifact() {
  source=$1
  destination=$2
  staged=$(mktemp "$install_dir/.codex-debug-install.XXXXXX")
  install -m 755 "$source" "$staged"
  mv -f "$staged" "$destination"
}

install_artifact "$binary" "$installed_binary"
install_artifact "$host_binary" "$installed_host_binary"
ln -sfn "$installed_binary" "$bin_dir/codex-debug"
ln -sfn "$installed_host_binary" "$bin_dir/codex-code-mode-host"

installed_version=$("$bin_dir/codex-debug" --version)
if [ "$installed_version" != "$expected_version" ]; then
  fail "installed binary reported '$installed_version'; expected '$expected_version'"
fi
"$bin_dir/codex-code-mode-host" --help >/dev/null

printf 'Built %s\n' "$actual_version"
printf 'Binary: %s\n' "$binary"
printf 'Code mode host: %s\n' "$host_binary"
printf 'Installed Codex debug: %s\n' "$installed_binary"
printf 'Installed code mode host: %s\n' "$installed_host_binary"
