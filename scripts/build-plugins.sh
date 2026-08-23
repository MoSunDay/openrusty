#!/usr/bin/env bash
# Build every gateway plugin (a directory under plugins/ containing a
# Cargo.toml) for wasm32-unknown-unknown and copy the cdylib artifact to
# build/plugins/<dir-name>.wasm.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="wasm32-unknown-unknown"

# Ensure the wasm target is installed (skip when rustup is unavailable,
# e.g. distro toolchains that already ship it).
if command -v rustup >/dev/null 2>&1; then
  if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    echo ">> installing rust target $TARGET"
    rustup target add "$TARGET"
  fi
fi

OUT_DIR="build/plugins"
mkdir -p "$OUT_DIR"

shopt -s nullglob

# Drop artifacts of plugins that were renamed or removed, so the output dir
# only ever contains wasm files matching a current plugins/<dir>.
for f in "$OUT_DIR"/*.wasm; do
  if [ ! -d "plugins/$(basename "$f" .wasm)" ]; then
    echo ">> removing stale artifact $f"
    rm -f "$f"
  fi
done

built=0
for dir in plugins/*/; do
  manifest="${dir}Cargo.toml"
  [ -f "$manifest" ] || continue
  name="$(basename "$dir")"

  # Package name -> cdylib file name ('-' becomes '_'). Crates overriding
  # [lib] name would need adjustment here.
  pkg="$(sed -n 's/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$manifest" | head -n 1)"
  if [ -z "$pkg" ]; then
    echo "!! cannot read package name from $manifest" >&2
    exit 1
  fi
  lib="${pkg//-/_}"

  echo ">> testing plugin '$name'"
  cargo test --manifest-path "$manifest"

  echo ">> building plugin '$name' (crate '$pkg')"
  cargo build --manifest-path "$manifest" --target "$TARGET" --release

  artifact="${dir}target/$TARGET/release/${lib}.wasm"
  if [ ! -f "$artifact" ]; then
    echo "!! missing artifact: $artifact" >&2
    exit 1
  fi
  cp "$artifact" "$OUT_DIR/$name.wasm"
  echo "   $OUT_DIR/$name.wasm: $(stat -c '%s' "$OUT_DIR/$name.wasm") bytes"
  built=$((built + 1))
done

if [ "$built" -eq 0 ]; then
  echo "!! no plugins found under plugins/" >&2
  exit 1
fi

echo "== built $built plugin(s):"
ls -l "$OUT_DIR"/*.wasm
