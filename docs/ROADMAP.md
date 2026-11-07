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

## M2 视频编码 + 自适应（省带宽第二位）

- [ ] NVENC H.264/HEVC 硬编（块难判定的区域走视频通道），QP 阶梯 28–34
- [ ] 帧率阶梯 30→20→15→10→0（静止停发）
- [ ] 码率闭环：按未确认字节 + 发送延迟调 target bitrate（先做简单 AIMD，不上 GCC）
- [ ] 降级顺序：降帧率 → 抬 QP → 降分辨率（1600×900 → 1280×720）

**验收**：1080p 视频播放 3–5 Mbps 可看；模拟 5% 丢包（netem/iptables 不可用则用发送侧人为丢弃）下不花屏 > 1 s。

## M3 观感与弱网（关键手感）

- [ ] 光标形状（`GetCursorInfo`+`CopyIcon`+`GetDIBits` → BGRA）独立消息
- [ ] LTR / 丢包时重发缓存帧，而不是请求 IDR
- [ ] 音频（WASAPI loopback + Opus 48k/32kbps）
- [ ] 剪贴板

## M4 传输层（把流量从云上赶走）

- [ ] QUIC 传输（`quinn`）替换 TCP，同一套 `Conn` API
- [ ] ICE 打洞 + TURN 兜底（自写轻量 STUN/TURN 客户端，或先做 UDP 打洞 + 服务端中继回退）
- [ ] 路径切换：direct_udp → relay_udp → relay_tcp，允许回切

**验收**：P2P 成功时 relay 侧只跑信令（云上带宽 ≈ 0）。

## 不做（明确砍掉）

- simulcast / SVC / SFU / 多租户
- COMPOUND 的 CUDA 内容分割（先用"两矩形 + 阈值"近似）
- 自建 IddCx 虚拟屏（无头机才需要）
- 完整复刻 zstd rect/palette/pixel_map 变换（先做 zstd + 左预测）
