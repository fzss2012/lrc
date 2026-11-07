#!/bin/sh
# 在 VPS 上配置 crates.io 国内镜像（直连 crates.io 在腾讯云上会卡住，实测 20 分钟没动）
set -e
mkdir -p "$HOME/.cargo"
cat > "$HOME/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
retry = 3
EOF
echo "=== ~/.cargo/config.toml ==="
cat "$HOME/.cargo/config.toml"
curl -s -o /dev/null -w "rsproxy reachable: %{http_code} (%{time_total}s)\n" --max-time 10 https://rsproxy.cn/index/config.json || true
