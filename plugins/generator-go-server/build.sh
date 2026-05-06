#!/usr/bin/env bash
# Build the generator-go-server plugin into plugin.wasm.
#
# Steps:
#   1. Stage `wit/` with deps from the shared forge:plugin package and
#      from TinyGo's bundled wasi:cli WIT — TinyGo finds wasi-cli on its
#      own at link time but `wit-bindgen-go` needs the deps physically
#      present to resolve includes.
#   2. Generate Go bindings into internal/bindings/.
#   3. Cross-compile via TinyGo's wasip2 target.
#
# Required toolchain (provided by `nix develop`):
#   - go      ≥ 1.22
#   - tinygo  ≥ 0.34   (wasip2 target; tested on 0.40.1)
#   - wit-bindgen-go (installed via `go install` into ./.gopath/bin)
#
# Output: ./plugin.wasm. Both plugin.wasm and the generated bindings are
# .gitignored — this script is the source of truth.

set -euo pipefail
cd "$(dirname "$0")"

# Serialize concurrent invocations (the integration-test suite spawns one
# build per test process). flock is part of util-linux; macOS ships it via
# the `util-linux` brew formula. If neither is present, fall back to a
# best-effort sentinel — concurrent runs will race but each is idempotent.
LOCK_FD=9
LOCK_FILE=".build.lock"
if command -v flock >/dev/null; then
    exec 9>"$LOCK_FILE"
    flock $LOCK_FD
fi

# Skip the rebuild if plugin.wasm is newer than every input file. After the
# lock is held, this lets N parallel test processes share a single build:
# the first does the work, the rest see a fresh artifact and exit.
if [ -f plugin.wasm ] && [ -z "${FORGE_FORCE_REBUILD:-}" ]; then
    newest_input=$(find main.go emit wit-source go.mod build.sh -type f -printf '%T@\n' 2>/dev/null | sort -nr | head -1)
    wasm_mtime=$(stat -c '%Y' plugin.wasm 2>/dev/null || stat -f '%m' plugin.wasm)
    if [ -n "$newest_input" ] && awk -v a="$wasm_mtime" -v b="$newest_input" 'BEGIN{exit !(a >= b)}'; then
        echo "build.sh: plugin.wasm is up to date; skipping (set FORGE_FORCE_REBUILD=1 to override)"
        exit 0
    fi
fi

GOPATH="$PWD/.gopath"
GOMODCACHE="$GOPATH/pkg/mod"
export GOPATH GOMODCACHE
export PATH="$GOPATH/bin:$PATH"

# Locate TinyGo's bundled wasi-cli WIT. Layout differs between distros:
#   - source / Nix:        <prefix>/share/tinygo/lib/wasi-cli/wit
#   - .deb (0.40+) install: /usr/local/lib/tinygo/lib/wasi-cli/wit
# Probe candidates rather than guess so the script works on every shape.
TINYGO_BIN="$(command -v tinygo)"
TINYGO_BIN_REAL="$(readlink -f "$TINYGO_BIN" 2>/dev/null || echo "$TINYGO_BIN")"
TINYGO_PREFIX="$(dirname "$(dirname "$TINYGO_BIN_REAL")")"
TINYGO_WIT_LIB=""
for candidate in \
    "$TINYGO_PREFIX/share/tinygo/lib/wasi-cli/wit" \
    "$TINYGO_PREFIX/lib/tinygo/lib/wasi-cli/wit" \
    "/usr/local/lib/tinygo/lib/wasi-cli/wit" \
    "/usr/lib/tinygo/lib/wasi-cli/wit"; do
    if [ -d "$candidate" ]; then
        TINYGO_WIT_LIB="$candidate"
        break
    fi
done
if [ -z "$TINYGO_WIT_LIB" ]; then
    echo "build.sh: cannot locate TinyGo's wasi-cli WIT (tried under $TINYGO_PREFIX and standard system paths)" >&2
    exit 1
fi

# Install wit-bindgen-go into the plugin-local GOPATH if missing.
if ! command -v wit-bindgen-go >/dev/null; then
    echo "build.sh: installing wit-bindgen-go into $GOPATH/bin"
    go install go.bytecodealliance.org/cmd/wit-bindgen-go@latest
fi

# Stage wit/ tree by copying. We copy rather than symlink so the layout is
# stable across machines (TinyGo's nix store path varies) and so that
# wasm-tools doesn't trip on symlinks during `wasm-tools component new`.
# `chmod -R +w` is required because TinyGo's bundled WIT files come from a
# read-only Nix store path, and the copied bits inherit those bits.
chmod -R +w wit 2>/dev/null || true
rm -rf wit
mkdir -p wit/deps/forge-plugin wit/deps/cli
cp wit-source/world.wit wit/world.wit
cp --no-preserve=mode ../../wit/*.wit wit/deps/forge-plugin/
cp --no-preserve=mode "$TINYGO_WIT_LIB"/*.wit wit/deps/cli/
for d in clocks filesystem io random sockets; do
    cp -r --no-preserve=mode "$TINYGO_WIT_LIB/deps/$d" wit/deps/
done

# Generate Go bindings.
rm -rf internal/bindings
wit-bindgen-go generate \
    --world code-generator-go \
    --out ./internal/bindings \
    --package-root github.com/MarcusDunn/openapi-forge/plugins/generator-go-server/internal/bindings \
    ./wit

# Component build.
tinygo build \
    -target=wasip2 \
    --wit-package ./wit \
    --wit-world code-generator-go \
    -o plugin.wasm \
    .

echo "build.sh: wrote $(stat -c %s plugin.wasm 2>/dev/null || stat -f %z plugin.wasm) bytes to plugin.wasm"
