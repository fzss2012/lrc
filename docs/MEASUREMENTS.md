# 实测数据（本机：RTX 3080 Ti / EPYC 7542 / 双 1920×1080）

> 复现命令都在文末。所有数字都是 `--release` 构建实测，不是估算。

## 1. M1 端到端（真实 DXGI 采集 + 真实客户端窗口）

| 场景 | 线上字节 | 备注 |
|---|---|---|
| **完全静止**（测试画面 static） | **0 B/s**（min=0，avg≈500 B/s 全是光标/ack） | 零变化零发送成立 |
| **连续打字**（每 112 ms 追加 1 字符） | **≈ 2.1 KB/s = 16.8 kbps** | 一次变化只发 1–2 个块 |
| **高速滚动**（全宽 400px 条带每帧上移） | ≈ 300 KB/s = **2.43 Mbps** | intra-only 的最坏情况；M2 用 P 帧解决 |
| **网页视频/照片流**（x.com 信息流自动播放） | 6–50 Mbps 剧烈波动 | 块内 JPEG 对视频内容必然爆；M2 换 H.264 |

帧率：host 采集 30–60 fps（`--fps` 可调），client 呈现 30–60 fps。
画面正确性：`PrintWindow`/屏幕合成截图与源屏逐区域一致，文字锐利（无损块生效）。

### 1.1 稳定性（30 秒纯静止长跑）

| 指标 | 结果 |
|---|---|
| 30 s 内线上字节 | **370 B**（≈15 B/s，全部是 ack/ping） |
| host 内存 / 句柄 | 61.0 MB → 61.0 MB；516 → 518 |
| client 内存 / 句柄 | 52.3 MB → 52.1 MB；542 → 545 |
| host CPU（30 s 累计） | 0.67 s ≈ 单核 2.2% |
| client CPU（30 s 累计） | 0.17 s ≈ 单核 0.6% |

### 1.2 M2 视频通道（mandelbrot 全屏运动画面，真实 host + 真实 client）

| 指标 | 实测 |
|---|---|
| 块模式（同内容） | 约 29–41 Mbps |
| **视频模式（HEVC NVENC 4 Mbps CBR）** | **2.2–3.1 Mbps** |
| 码率降幅 | **≈10×** |
| 客户端管线延迟 `lat_ms` | **42–48 ms**（含 NVENC 编码 ~20 ms + 客户端软解 ~14 ms） |
| 视频帧率 | 15–21 fps（受 ffplay 的 mandelbrot 生成速度限制，不是链路限制） |
| 模式切换 | 播完 1–2 秒内自动切回 `mode=blocks`，客户端同步 |
| 码流合法性 | host 侧把 AU dump 后 `ffprobe` → `codec_name=hevc 1920x1080 nb_read_frames=92`，与发出的 AU 数一致 |

### 1.3 全量验收（`scripts/acceptance.ps1`，2026-11-07 最终一轮：7/7 全通过）

```
[PASS] release 构建        — cargo build --release --workspace OK
[PASS] 单元测试            — 199 个测试通过 / exit=0
[PASS] 静止零发送          — min=0 B/s
[PASS] 办公打字 ≤1.0 Mbps  — avg=373.4 kbps
[PASS] 视频模式（码率受控） — mode=video 11/12 秒, video_kbps avg=837
[PASS] 视频管线延迟 <60ms  — lat_ms avg=41.6（21 个采样）
[PASS] 停播后自动回块模式   — blocks 3/3
```

### 1.3.1 最终验收（`scripts/acceptance.ps1`，2026-11-07 收尾轮：5/5 + 构建 + 199 单测）

```
[PASS] 静止零发送            — min=0 B/s
[PASS] 办公打字 ≤1.0 Mbps    — avg=216.3 kbps
[PASS] 视频模式（码率受控）   — mode=video 12/12 秒, video_kbps avg=866
[PASS] 视频管线延迟中位数<60ms — median=32.6 ms, p90=40.1 ms
[PASS] 停播后自动回块模式     — 4/4
```

### 1.4 M4 传输层（`scripts/verify-p2p.ps1`，本机 loopback + 真 relay）

| 阶段 | host path | client path | relay 转发字节 | 说明 |
|---|---|---|---|---|
| `--p2p auto` | **direct_udp** | **direct_udp** | **0** | 打洞成功，**媒体完全不经过 relay**（云上只跑信令） |
| `--p2p relay` | **relay_udp** | **relay_udp** | **1,854,501 B** | 强制降级，relay 正常转发 |

- direct 路径实测：`udp_rtt 14–23 ms`、`udp_loss 0`、30 fps、3,117 块解码正常
- relay 路径实测：`udp_rtt 2–12 ms`、`udp_loss 23–39‰`（relay 单线程转发在忙时会丢包，靠 `lrc-udp` 重传补回，画面不烂）
- `lrc-udp` 自测：回环 1000 条消息 15.1 MB/s；**注入 10% 丢包仍 1000 条全量有序不重复**（重传 51.9%）

### 1.5 M4 弱网验收（`scripts/verify-p2p-lossy.ps1`：udp-proxy 注入丢包 + 20ms 延迟）

| 丢包设定 | path | fps | 解码块 | 码率(client) | udp_rtt | udp_loss | 重连 | relay 转发 |
|---|---|---|---|---|---|---|---|---|
| 0% | relay_udp | 7 | 6,188→9,463 | 1,453 kbps | 58.8 ms | 0‰ | 0 | 2.7 MB |
| 5% | relay_udp | 7 | 6,072→8,850 | 1,547 kbps | 59.6 ms | **46‰** | 0 | 4.3 MB |
| 10% | relay_udp | 7 | 6,040→8,824 | 1,501 kbps | 55.1 ms | **70‰** | 0 | 5.1 MB |

- **会话全程不中断**，码率最差只为基线 1.06×（要求 <1.5×）
- **断线重连**：全丢 14 秒 → 客户端「链路保活超时」判死 → 重连 1 次 → 恢复 **fps 30**
- 首屏优化：大块帧拆成 ≤32 KB 多条消息后，5% 丢包下首屏从 **5 s → 1 s**

## 2. 采集（`capture-probe`）

| 指标 | 数值 |
|---|---|
| 静止态 grab() 返回 None 比例 | **99.9%**（3 s 内 331,954 次调用只有 345 帧；timeout=0 时 idle 单次 4 µs） |
| 有帧时采集耗时 | avg 3.4 ms（map 0.8 ms + blit 0.4 ms，其余在等桌面更新） |
| dirty rect | avg 1.2/帧，max 4；99.7% 的帧走"只回读脏区并集" |
| 块哈希 | 510 块 1049 µs；50 块 84 µs（按脏区限哈希后基本可忽略） |
| 光标轮询 | 无变化 3–18 µs；抓形状 176–532 µs（仅句柄变化时） |

## 3. 编码（`lrc-bench`，真实桌面帧 1920×1080，510 块）

| 场景 | 变化块 | 每帧字节 | 折算 @30fps | 编码耗时 |
|---|---|---|---|---|
| static | 0 | **0** | 0 | 0.9 ms |
| typing（重负载版） | 16.1 | 4,245 | 1.02 Mbps | 1.8 ms |
| scroll（每帧 7px） | 58.1 | 27,666 | 6.64 Mbps | 4.5 ms |
| full（整屏重编） | 298.5 | 119,998 | 28.8 Mbps | 16.5 ms |
| video（整屏视频） | 510 | 1,313,123 | 315 Mbps | **174 ms（5.7 fps）** |

**策略对比（同一批脏块）**：混合策略 vs 全 JPEG → typing 省 **5.44×**、scroll 2.93×、full 3.14×；
但 video 场景混合反而差 2.7×（平滑图像块无损不划算）→ `lossless_ratio_limit` 由 0.55 收到 0.20。

**结论**：intra-only 块编码够用且非常适合办公/文字，但**视频类内容必须走 NVENC H.264/HEVC**（CPU 单线程 JPEG 对 1080p 全屏只有 5.7 fps）。

## 4. 与调研报告预期的对照

| 项 | 报告预期（§5.2） | 实测 | 结论 |
|---|---|---|---|
| 办公 idle | ≈0 | **0 B/s** | ✅ 达成 |
| 办公打字 | 0.3–1.0 Mbps | **16.8 kbps** | ✅ 远优于预期（只发真正变化的块） |
| 网页滚动 | 1.5–3 Mbps | 2.4 Mbps（实际 e2e）/ 6.6 Mbps（bench 重负载） | ⚠️ 边缘，需 M2 |
| 1080p 视频 | 3–6 Mbps | 6–50 Mbps | ❌ 必须 M2（H.264 硬编） |

## 5. 复现命令

```powershell
cd D:\lrc
cargo build --release
# 采集层
target\release\capture-probe.exe --display 1 --seconds 3 --timeout 0
# 编码层（离线基准，60 秒）
cargo run --release -p lrc-bench -- --frames 120
# 端到端像素 + 静止码率
powershell -NoProfile -File scripts\verify-e2e-v2.ps1 -HostDisplay 1 -ObserveSeconds 8
# 办公场景带宽（会用测试画面覆盖被采集那块屏）
powershell -NoProfile -File scripts\measure-phase.ps1 -Mode typing -HostDisplay 1
```
