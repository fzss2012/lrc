# lrc — 轻量自研远控（B 路线）

按 `ToDesk-逆向调研与自建远控方案.md` §6 的 B 路线实现：**只做单流点对点**，
核心三件事优先于编码器选型。

1. **零变化零发送**：DXGI dirty rect + 块哈希，静止时线上 0 字节
2. **块级脏区**：64×64 块，未变块不发
3. **文字无损 / 图像有损逐块分流**：zstd(左预测) ⇄ JPEG，按块自动选小者

## 实测（本机 1080p 双屏，详见 docs/MEASUREMENTS.md）

| 场景 | 实测带宽 |
|---|---|
| 完全静止 | **0 B/s** |
| 连续打字 | **16.8 kbps** |
| 高速滚动 | 2.4 Mbps |
| 网页视频流 | 6–50 Mbps（M2 换 H.264 硬编） |

## 布局

| crate | 职责 |
|---|---|
| `lrc-proto` | 线格式（帧/消息/握手/输入事件），无第三方依赖 |
| `lrc-codec` | 块编码：无损(zstd+左预测) / 有损(JPEG) / raw |
| `lrc-capture` | Windows DXGI Desktop Duplication 采集 + dirty/move rect + 块哈希 + 光标 |
| `lrc-render` | D3D11 呈现 |
| `lrc-transport` | 长度前缀帧收发（TCP 起步，后续 QUIC） |
| `lrc-host` | 被控端：采集→差分流→编码→发送；回放输入（SendInput） |
| `lrc-client` | 主控端：窗口 + 收流→解码→呈现；采集本地输入回传 |
| `lrc-bench` | 离线带宽基准（真实桌面帧 + 五种场景） |
| `lrc-video` | （M2）NVENC 视频通道 |

设计与线格式见 `docs/DESIGN.md`，里程碑见 `docs/ROADMAP.md`，
使用手册见 `docs/USAGE.md`，实测数据见 `docs/MEASUREMENTS.md`，
**Linux（X11）被控端/控制端见 `docs/LINUX.md`**，
**VPS 部署（lrc-relay）见 `docs/VPS-DEPLOY.md`**（含本次腾讯云部署的真实记录与排障清单）。

## 快速开始

```powershell
cargo build --release
# 被控端（需要能看到桌面/被采集的屏）
target\release\lrc-host.exe --listen 0.0.0.0:7800 --password 1234 --display 0 --stats
# 主控端
target\release\lrc-client.exe --connect 127.0.0.1:7800 --password 1234 --stats
```

## 验收脚本

```powershell
scripts\verify-e2e-v2.ps1 -HostDisplay 1 -ObserveSeconds 8     # 像素 + 静止码率
scripts\measure-phase.ps1 -Mode typing -HostDisplay 1           # 办公场景带宽
target\release\capture-probe.exe --display 1 --seconds 3 --timeout 0
cargo run --release -p lrc-bench -- --frames 120
```
