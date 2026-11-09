# Linux（X11）被控端/控制端

Windows 版的 `lrc-host` / `lrc-client` 依赖 DXGI/D3D11/Win32，不能直接在 Linux 跑。
`crates/lrc-linux` 只重写**平台相关**的三块，协议/编码/传输/视频全部复用：

| 复用（未改一行） | 新写（lrc-linux） |
|---|---|
| lrc-proto 线格式、lrc-codec 块编码、lrc-transport/udp/relay 传输与 P2P | capture.rs（x11rb + MIT-SHM）、input.rs（XTEST）、cursor.rs（XFixes） |
| | lrc-host-linux（被控端）、lrc-client-linux（控制端，x11rb 窗口） |

两端可以**互连**：Windows ↔ Linux 任意方向都行。

## 实测环境：yanfa2

| 项 | 值 |
|---|---|
| 系统 | Ubuntu 24.04.4 LTS，内核 6.17，**X11**（Xorg 在 `:1`） |
| 硬件 | 32 核 / 64 GB / RTX 5090 |
| 屏幕 | **3840×1080 单个 X root**（双屏被 X 合并），60×17 块网格 |
| 采集 | MIT-SHM **3.5 ms/帧**（回退 GetImage 6.95 ms，`LRC_NO_SHM=1` 可强制回退） |
| 光标 | XFixes `GetCursorImage`，不动时 0 上报；poll 约 90–120 µs |
| 输入 | XTEST（含指针加速补偿、keysym→keycode 临时映射） |

## 部署（yanfa2 已装好）

两个 **systemd 用户服务**（`~/.config/systemd/user/`）：

| 服务 | 作用 | 端口 |
|---|---|---|
| `lrc-host-linux` | 被控端，采集 `:1` 推流 | `127.0.0.1:7800` |
| `lrc-x11vnc` | 图形通道（VNC，可选） | `127.0.0.1:5900` |

二进制在 `~/lrc/bin/`，日志 `~/lrc/host.log` / `~/lrc/x11vnc.log`。
两个端口都**只绑 127.0.0.1**，外部访问一律走 SSH 隧道。

## 怎么用

### 从 Windows 控 yanfa2（日常用法）

```powershell
cd D:\lrc
# 1) 建立隧道（把 yanfa2 的 7800 映射到本机 7800）
ssh -F C:\Users\sss\.ssh\lrc-ssh-config -N -L 7800:127.0.0.1:7800 yanfa2

# 2) 另一个窗口起客户端
target\release\lrc-client.exe --password 1234 --connect 127.0.0.1:7800 --stats
```

### 从 yanfa2 控 Windows

```bash
# 反向隧道：yanfa2:17800 → 本机 7805
ssh -F ... -N -R 17800:127.0.0.1:7805 yanfa2@127.0.0.1   # 在 Windows 侧执行
# yanfa2 上：
DISPLAY=:1 XAUTHORITY=/run/user/1000/gdm/Xauthority ~/lrc/bin/lrc-client-linux --password 1234 --connect 127.0.0.1:17800 --stats
```

### 跨公网 P2P（走 VPS 信令/中继）

两边都加 `--signal 118.24.79.47:7801 --room <N> --p2p auto`，实测得到 `relay_udp`、udp_rtt 78–93 ms。

## ⚠️ VPS 上的一个坑（已修）

VPS 上跑着 `sing-box-client.service`（TUN + 策略路由 table 2022，默认所有流量走隧道去首尔）。
relay 的 UDP 回包也被送进隧道 → 对端收不到响应 → **REGISTER/CONNECT 一律超时**。

修复：加一条高优先级规则，让**源端口 7801** 的包走 main 表（物理网卡）：

```bash
sudo ip rule add sport 7801 lookup main pref 8998
```

已固化为 `/etc/systemd/system/lrc-relay-routing.service`（开机自动生效）。
**如果以后 VPS 上换了代理方案，这条规则要重新确认。**

## 本期没做

- **音频**（lrc-audio 是 WASAPI，Linux 要重写 PulseAudio/PipeWire）
- 视频通道（NVENC）、自适应降级、多显示器切换（Linux 端目前只有块模式、单 X root）
- **Wayland**（只支持 X11；Wayland 会话下采集要换 PipeWire+portal）

## 构建/测试

```powershell
# 同步源码到 yanfa2 并构建
powershell -NoProfile -File scripts\remote\linux-build.ps1
powershell -NoProfile -File scripts\remote\linux-build.ps1 -CargoArgs "test -p lrc-linux"
# 一键安装（构建 + 装 bin + systemd 用户服务）
powershell -NoProfile -File scripts\remote\install-linux-clients.ps1
```
