# 通过共用 VPS 访问内网机器（yanfa2）

不依赖 lrc，纯 SSH 反向隧道。前提：**目标机器能出网到 VPS**（有公网或能 NAT 出去即可），VPS 上 `allowtcpforwarding yes`（默认就是）。

## 拓扑

```
本机 ──ssh──▶ VPS(118.24.79.47) ──▶ 127.0.0.1:2222 ──隧道──▶ yanfa2:22
                                    └▶ 127.0.0.1:5900 ──隧道──▶ yanfa2:5900 (VNC，可选)
```

VPS 上 `gatewayports no`，所以隧道只监听 **VPS 自己的 127.0.0.1**，不会把 yanfa2 暴露到公网 —— 这正是我们要的。

## 三步

**1) 在 yanfa2 上放好"连 VPS 的私钥"**

用你已经在用的、VPS 认可的私钥（例如 DSH 隧道那把 `dsh_chengdu_tunnel`），或者新生成一把并把公钥加到 VPS 的 `~/.ssh/authorized_keys`。

**2) 在 yanfa2 上跑隧道**

```bash
# 拷过去
scp yanfa2-tunnel.sh yanfa2:~/
# 前台试跑（Ctrl-C 退出）
KEY=~/.ssh/id_ed25519 bash ~/yanfa2-tunnel.sh
```

常驻（可选，用 systemd）：

```bash
sudo tee /etc/systemd/system/yanfa2-tunnel.service >/dev/null <<'EOF'
[Unit]
Description=reverse SSH tunnel to VPS
After=network-online.target
Wants=network-online.target

[Service]
User=yanfa2
Environment=VPS_HOST=118.24.79.47
Environment=KEY=/home/yanfa2/.ssh/id_ed25519
ExecStart=/bin/bash /home/yanfa2/yanfa2-tunnel.sh
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload && sudo systemctl enable --now yanfa2-tunnel
sudo journalctl -u yanfa2-tunnel -f
```

**3) 在本机连接**

```powershell
cd D:\lrc
powershell -NoProfile -File scripts\remote\connect-yanfa2.ps1
# 或直接跑一条命令
powershell -NoProfile -File scripts\remote\connect-yanfa2.ps1 -Command "hostname; uname -a"
```

底层其实就是：

```
ssh -J ubuntu@118.24.79.47 -i C:\Users\sss\.ssh\key117.pem -p 2222 yanfa2@127.0.0.1
```

## 想要图形界面

在 yanfa2 上装并启动 `x11vnc`（X11 会话）：

```bash
sudo apt install -y x11vnc
x11vnc -localhost -forever -shared -rfbport 5900 -display :0 &
```

然后本机用任意 VNC 客户端连 **`127.0.0.1:5900` 经 VPS 转发**：

```powershell
ssh -i C:\Users\sss\.ssh\key117.pem -J ubuntu@118.24.79.47 -L 5900:127.0.0.1:5900 ubuntu@118.24.79.47 -N
# 保持这个窗口开着，再用 VNC 客户端连 127.0.0.1:5900
```

> Wayland 会话下 x11vnc 不可用，需要走 PipeWire+portal 或用 `wayvnc`；先确认 `echo $XDG_SESSION_TYPE`。

## 注意

- 这条链路**没有额外加密层**（SSH 本身加密，OK），但 VPS 上是明文可见的管理面；
- 反代端口只绑 VPS 的 127.0.0.1，公网扫不到；
- yanfa2 重启后隧道会断，用上面的 systemd 单元可以自愈。
