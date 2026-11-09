# 通过共用 VPS 访问内网机器（yanfa2）

不依赖 lrc，纯 SSH 反向隧道。**已实测跑通（2026-11-09）**。

## 已确认的环境

| 项 | 值 |
|---|---|
| 主机 | `yanfa2`，Ubuntu 24.04.4 LTS，内核 6.17.0-35 |
| 硬件 | **32 核 / 64 GB / RTX 5090（驱动 580.159.03）** |
| 桌面 | **X11**（Xorg 在 `:1`，gnome-shell 在跑；**不是 Wayland**） |
| 磁盘 | 1.8 T，**已用 98%，剩 44 G**（注意） |
| 登录用户 | `yanfa2`（sudo / docker 组，sudo 免密） |

## 拓扑

```
本机 ──ssh──▶ VPS(118.24.79.47) ──▶ 127.0.0.1:2222 ──反向隧道──▶ yanfa2:22
                                  └▶ 127.0.0.1:5900 ──反向隧道──▶ yanfa2:5900 (x11vnc)
```

VPS 上 `gatewayports no`，隧道只绑 **VPS 自己的 127.0.0.1**，公网扫不到。

## 在 yanfa2 上（一次性）

```bash
# 1) 私钥：~/yanfa2_tunnel 或 ~/.ssh/lrc_tunnel，权限必须 600
chmod 600 ~/yanfa2_tunnel

# 2) 起隧道（自带断线重连）
KEY=~/yanfa2_tunnel bash ~/yanfa2-tunnel.sh &

# 3) 图形界面：装并启动 x11vnc（只监听本机回环）
sudo apt-get install -y x11vnc
x11vnc -display :1 -auth /run/user/1000/gdm/Xauthority \
       -localhost -forever -shared -rfbport 5900 -noxdamage -nopw &
```

> `-nopw` 可以接受，因为 x11vnc 只监听 127.0.0.1；要更保险加 `-rfbauth ~/.vnc/passwd`。

## 在本机

```powershell
cd D:\lrc
# 开一个 shell
powershell -NoProfile -File scripts\remote\connect-yanfa2.ps1
# 跑单条命令
powershell -NoProfile -File scripts\remote\connect-yanfa2.ps1 -Command "uptime; nvidia-smi"
# 图形界面（先装个 VNC 客户端：TightVNC / UltraVNC / RealVNC Viewer 都行）
powershell -NoProfile -File scripts\remote\connect-yanfa2.ps1 -Vnc
# 然后 VNC 客户端连 127.0.0.1:5900
```

## 常驻（systemd）

```bash
sudo tee /etc/systemd/system/yanfa2-tunnel.service >/dev/null <<'EOF'
[Unit]
Description=reverse SSH tunnel to VPS
After=network-online.target
Wants=network-online.target

[Service]
User=yanfa2
Environment=VPS_HOST=118.24.79.47
Environment=KEY=/home/yanfa2/yanfa2_tunnel
ExecStart=/bin/bash /home/yanfa2/yanfa2-tunnel.sh
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload && sudo systemctl enable --now yanfa2-tunnel
```

## 坑（都踩过）

| 现象 | 原因 | 解决 |
|---|---|---|
| `bad permissions` / `Permission denied (publickey)` | 私钥权限 0644 | `chmod 600` |
| 隧道跑起来但 VPS 上没端口 | 认证失败（preauth 断连） | 看 VPS `/var/log/auth.log` 里 `from <IP> [preauth]` |
| VNC 连不上但 5900 在听 | x11vnc 没起，或 display 不是 `:0` | 这台是 **X1**，要 `-display :1` |
| x11vnc 启动报 cannot open display | 缺 Xauthority | 带上 `-auth /run/user/1000/gdm/Xauthority` |
