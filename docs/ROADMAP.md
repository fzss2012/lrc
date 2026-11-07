# 里程碑

对齐 `ToDesk-逆向调研与自建远控方案.md` §6.3，但把"验收"落到可测量的命令上。
**M1 是唯一必须立刻完成的目标**；后面每一条都标了它到底省多少带宽，方便砍。

## M1 最小可用（已完成，实测见 docs/MEASUREMENTS.md）

- [x] DXGI 采集 + dirty rect（`lrc-capture`）
- [x] 64×64 块哈希差分（`lrc-host`）
- [x] 文字块无损（zstd+左预测）/ 图像块 JPEG，逐块取小（`lrc-codec`）
- [x] TCP 长度前缀传输 + ack 流控（`lrc-transport`）
- [x] SendInput 输入回放（`lrc-host`）
- [x] 主控端窗口 + 呈现 + 输入转发（`lrc-client`/`lrc-render`）
- [x] 光标位置/形状消息（host 发送；客户端本地合成 —— 本轮进行中）

**验收（本机双屏实测）**

| 项 | 命令/方法 | 标准 |
|---|---|---|
| 静止零发送 | host `--stats` 观察 idle 期间 `kbps=0.0`、`sent` 不增长 | 连续 10 s 增长 < 1 KB（只有 ping/ack） |
| 办公码率 | 在 DISPLAY1 里打字/滚动，读 host `--stats` | 平均 ≤ 1.0 Mbps |
| 画面正确 | 主控端窗口截图 vs 源屏截图逐块比对 | 无损块完全一致 |
| 延迟 | `Ping/Pong` RTT + 手测跟手度 | LAN < 60 ms |

## M2 视频编码 + 自适应（已完成）

- [x] NVENC H.264/HEVC 硬编（整帧视频通道），实测 10× 码率下降
- [x] 帧率阶梯 30→20→15→10→2（`lrc-adapt`，10 档 + 滞回）
- [x] 码率闭环：丢帧/RTT/未确认字节/码率 四类信号驱动升降档
- [x] 降级顺序：降帧率 → 抬 QP → 降分辨率（1600×896 → 1280×704）→ 近停发
- [x] 断线自动重连（指数退避 1/2/4/8/10s）
- [x] 采集零拷贝 `grab_view()`（省 2 ms/帧、243 MB/s）
- [x] 光标端到端（形状+位置+本地合成，无残影）
- [x] 首帧全零修复（重试 + GDI 兜底，7/10 → 0/10）

**验收（已通过）**：`scripts/acceptance.ps1` 7/7；视频模式 800–3000 kbps、延迟 41 ms；静止 0 B/s。

## M3 观感与弱网（音频已完成，其余待做）

- [x] 光标形状（`GetCursorInfo`+`CopyIcon`+`GetDIBits` → BGRA）独立消息 + 客户端合成
- [x] 音频（WASAPI loopback + Opus，实测 38 kbps、静音 0 字节、抖动缓冲 50–60 ms）
- [x] 多显示器切换（`DisplayList`/`SelectDisplay`，Ctrl+Alt+数字）
- [ ] LTR / 丢包时重发缓存帧，而不是请求 IDR
- [ ] 剪贴板

## M4 传输层（已按自研方案完成 —— 没用 QUIC/ICE，而是更轻的等价物）

- [x] 可靠有序 UDP 消息传输（`lrc-udp`：分片/ACK/NACK/RTO 重传/保活，语义等价 TCP 流）
- [x] 自研信令 + 打洞 + 中继（`lrc-relay`：房间登记 / 地址交换 / UDP 中继 / 有损模拟器）
- [x] 路径阶梯：`direct_udp` → `relay_udp` → `tcp`，客户端自动降级并按"上次成功路径"优先重试
- [x] `lrc-transport::link` 统一抽象，上层协议（握手/帧/音频/视频）零改动
- [ ] 更完整的 NAT 穿越（多候选地址、ICE-lite）—— 当前是"单地址打洞 + relay 兜底"，覆盖绝大多数家庭 NAT

**验收（已通过）**：`scripts/verify-p2p.ps1` → `--p2p auto` 两端 **direct_udp** 且 **relay 转发 0 字节**（云上只跑信令）；
`--p2p relay` 降级正常转发 1.85 MB。

## 不做（明确砍掉）

- simulcast / SVC / SFU / 多租户
- COMPOUND 的 CUDA 内容分割（先用"两矩形 + 阈值"近似）
- 自建 IddCx 虚拟屏（无头机才需要）
- 完整复刻 zstd rect/palette/pixel_map 变换（先做 zstd + 左预测）
