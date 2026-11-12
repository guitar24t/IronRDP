#!/usr/bin/env bash
# Builds the browser package (@devolutions/iron-remote-desktop-rdp layout: one ES module with
# the WebAssembly inlined, plus its type declarations) into wrhs/dist.
# Same steps as `cargo xtask web build`, without building the Svelte demo client.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/crates/ironrdp-web"
# Paths as rustc sees them (Windows paths under Git Bash), mapped to fixed names so the build
# does not embed the local directories.
native() { if command -v cygpath >/dev/null; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
remap="--remap-path-prefix=$(native "$root")=/IronRDP"
remap+=" --remap-path-prefix=$(native "${CARGO_HOME:-$HOME/.cargo}")=/cargo"
remap+=" --remap-path-prefix=$(native "${RUSTUP_HOME:-$HOME/.rustup}")=/rustup"
export RUSTFLAGS="-Ctarget-feature=+simd128,+bulk-memory --cfg getrandom_backend=\"wasm_js\" -Copt-level=s -Ccodegen-units=1 -Cllvm-args=-enable-dfa-jump-thread $remap"
# WASM_PACK_PROFILE=--profiling keeps function names (for profiling only; not for release).
wasm-pack build ${WASM_PACK_PROFILE:-} --target web
js=pkg/ironrdp_web.js
# Vite inlines the WebAssembly when it is imported with ?url (see xtask/src/web.rs).
node -e '
const fs = require("fs"); const p = process.argv[1];
let s = fs.readFileSync(p, "utf8");
if (!s.startsWith("import wasmUrl")) s = "import wasmUrl from \"./ironrdp_web_bg.wasm?url\";\n\n" + s;
s = s.replace("new URL(\x27ironrdp_web_bg.wasm\x27, import.meta.url)", "wasmUrl");
fs.writeFileSync(p, s);' "$js"
cd "$root/web-client/iron-remote-desktop-rdp"
[ -d node_modules ] || npm ci --no-audit --no-fund
npx vite build
out="$root/wrhs/dist"
rm -rf "$out" && mkdir -p "$out"
cp dist/iron-remote-desktop-rdp.js dist/index.d.ts "$out/"
( cd "$out" && sha256sum iron-remote-desktop-rdp.js index.d.ts )
