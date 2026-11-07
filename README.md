# lrc — 轻量自研远控（B 路线）

按 `ToDesk-逆向调研与自建远控方案.md` §6 的 B 路线实现：**只做单流点对点**，
核心三件事优先于编码器选型。

1. **零变化零发送**：DXGI dirty rect + 块哈希，静止时线上 0 字节
2. **块级脏区**：64×64 块，未变块不发
3. **文字无损 / 图像有损逐块分流**：zstd(左预测) ⇄ JPEG，按块自动选小者

## 布局

| crate | 职责 |
|---|---|
| `lrc-proto` | 线格式（帧/消息/握手/输入事件），无第三方依赖 |
| `lrc-codec` | 块编码：无损(zstd+左预测) / 有损(JPEG) / raw |
| `lrc-capture` | Windows DXGI Desktop Duplication 采集 + dirty/move rect + 块哈希 |
| `lrc-render` | D3D11 呈现 + 光标合成 |
| `lrc-transport` | 长度前缀帧收发（TCP 起步，后续 QUIC） |
| `lrc-host` | 被控端：采集→差分流→编码→发送；回放输入（SendInput） |
| `lrc-client` | 主控端：窗口 + 收流→解码→呈现；采集本地输入回传 |

设计与线格式见 `docs/DESIGN.md`。

## 快速开始

```powershell
cargo build --release
# 被控端（需要能看到桌面/被采集的屏）
cargo run --release -p lrc-host -- --listen 0.0.0.0:7800 --password 1234
# 主控端
cargo run --release -p lrc-client -- --connect 127.0.0.1:7800 --password 1234
```

## 验收（M1）

- 局域网 1080p 办公可用（打字/滚动/拖窗口）
- **静止时 0 字节**（`lrc-host --stats` 输出 idle 期间 sent=0）
- 办公场景平均码率 ≤ 1.0 Mbps
