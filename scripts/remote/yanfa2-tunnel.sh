#!/bin/bash
# 在 yanfa2（Linux）上运行：把本机 SSH / VNC 反向映射到共用 VPS 的 127.0.0.1
#
# 用法（在 yanfa2 上）：
#   bash yanfa2-tunnel.sh                 # 用默认参数
#   KEY=~/.ssh/id_ed25519 VNC=1 bash yanfa2-tunnel.sh
#
# 环境变量：
#   VPS_USER  默认 ubuntu
#   VPS_HOST  默认 118.24.79.47
#   KEY       连接 VPS 的私钥（默认 ~/.ssh/id_ed25519）
#   SSH_PORT  VPS 上的反向端口（本机 22 映射过去），默认 2222
#   VNC_PORT  VPS 上的反向端口（本机 5900 映射过去），默认 5900；设为空字符串则不转发
#   RETRY     断线重连间隔秒，默认 5
set -u
VPS_USER="${VPS_USER:-ubuntu}"
VPS_HOST="${VPS_HOST:-118.24.79.47}"
KEY="${KEY:-$HOME/.ssh/id_ed25519}"
SSH_PORT="${SSH_PORT:-2222}"
VNC_PORT="${VNC_PORT:-5900}"
RETRY="${RETRY:-5}"

if [ ! -f "$KEY" ]; then
  echo "找不到私钥 $KEY，请先用 KEY=/path/to/key 指定" >&2
  exit 1
fi

log() { echo "[$(date '+%F %T')] $*"; }

while true; do
  ARGS=(-N -i "$KEY"
        -o ServerAliveInterval=15 -o ServerAliveCountMax=3
        -o TCPKeepAlive=yes -o ExitOnForwardFailure=yes -o BatchMode=yes
        -o StrictHostKeyChecking=accept-new
        -R "127.0.0.1:$SSH_PORT:127.0.0.1:22")
  if [ -n "$VNC_PORT" ]; then
    ARGS+=(-R "127.0.0.1:$VNC_PORT:127.0.0.1:5900")
  fi
  log "连接 $VPS_USER@$VPS_HOST：ssh=$SSH_PORT${VNC_PORT:+ vnc=$VNC_PORT}"
  ssh "${ARGS[@]}" "$VPS_USER@$VPS_HOST"
  log "隧道断开（exit=$?），${RETRY}s 后重连"
  sleep "$RETRY"
done
