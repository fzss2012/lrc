#!/bin/sh
# 在 VPS 上安装 Rust（用中科大镜像装 rustup 本体）
set -e
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable --no-modify-path
export PATH="$HOME/.cargo/bin:$PATH"
echo "=== versions ==="
rustc --version
cargo --version
