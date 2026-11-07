#!/bin/sh
# 在 VPS 上构建 lrc-relay（前提：Rust 已装、源码已解压到 ~/lrc/relay-src）
set -e
export PATH="$HOME/.cargo/bin:$PATH"
cd "$HOME/lrc/relay-src"
cargo build --release -p lrc-relay
echo "=== 产物 ==="
ls -la target/release/lrc-relay
