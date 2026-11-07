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
