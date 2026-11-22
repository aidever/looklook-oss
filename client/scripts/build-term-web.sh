#!/usr/bin/env bash
# 编译浏览器端终端成 WASM（随仓库提交，`npm run build` 不需要 Rust 工具链）：
# - crates/term-web → src-term/dom/wasm/（手机 DOM 渲染）
# - crates/term-gpu → src-term/gpu/wasm/（电脑 wgpu 渲染，里面也带着 term-web 的 TermClient）
# 改了 crates/term-proto、term-web 或 term-gpu 之后跑一次：npm run build:wasm
# 需要：rustup target add wasm32-unknown-unknown；wasm-bindgen-cli 与 Cargo.lock 里的 wasm-bindgen 同一版本（0.2.130）。
set -euo pipefail
cd "$(dirname "$0")/.."
target_dir="${CARGO_TARGET_DIR:-target}"
want="$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
have="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}')"
if [ "$want" != "$have" ]; then
  echo "wasm-bindgen-cli 版本不对：需要 $want，现在是 ${have:-没装}（cargo install wasm-bindgen-cli --version $want）" >&2
  exit 1
fi
cargo build --release --locked --target wasm32-unknown-unknown -p term-web -p term-gpu
wasm-bindgen --target web --no-typescript --out-dir src-term/dom/wasm --out-name term_web \
  "$target_dir/wasm32-unknown-unknown/release/term_web.wasm"
wasm-bindgen --target web --no-typescript --out-dir src-term/gpu/wasm --out-name term_gpu \
  "$target_dir/wasm32-unknown-unknown/release/term_gpu.wasm"
ls -l src-term/dom/wasm src-term/gpu/wasm
