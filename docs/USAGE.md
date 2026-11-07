# lrc 使用手册

> 本手册内容来自代码事实（`crates/*/src/*.rs`、`Cargo.toml`、`scripts/*.ps1`、`docs/*.md`）。与代码不一致时以代码为准。

lrc 是一个**单流点对点**的 Windows 远控：被控端 `lrc-host` 采集桌面并发流，主控端 `lrc-client` 开窗口收流、本地渲染、把鼠标键盘回传。三条设计优先级：

1. **零变化零发送**：静止时线上 0 字节（连帧头都不发）
2. **块级脏区**：64×64 块网格，未变化块不发
3. **文字无损 / 图像有损逐块分流**：zstd(左预测) ⇄ JPEG ⇄ 调色板，逐块取最小

设计/线格式见 `docs/DESIGN.md`，里程碑见 `docs/ROADMAP.md`，实测数字见 `docs/MEASUREMENTS.md`。

---

## 1. 构建与依赖

### 1.1 环境要求

| 项 | 要求 |
|---|---|
| 操作系统 | **仅 Windows**（DXGI Desktop Duplication / D3D11 / WASAPI / SendInput） |
| Rust | 开发用 **1.93.1 MSVC**；workspace 的 `rust-version = 1.82` |
| 工具链 | `x86_64-pc-windows-msvc` + MSVC 链接器（VS Build Tools） |
| 显卡 | 块模式不需要独显；视频通道需要 **NVIDIA NVENC**（HEVC/H.264） |
| 屏幕 | SDR 桌面；HDR/10bit 例外见 §6.3 |
| 第三方 | **ffmpeg**（视频通道、音频通道、验收脚本里的 ffplay），见 1.2 |

### 1.2 ffmpeg：谁依赖它、没有它会怎样

| 功能 | 依赖 ffmpeg | 没有 ffmpeg / 没有 NVENC 时的行为 |
|---|---|---|
| 块模式（默认路径） | 不依赖 | 正常工作（纯 Rust：zstd + jpeg-encoder） |
| 视频通道（`--video auto/hevc/h264`） | 依赖 | 会话建立时**探测失败 → 打一次告警 → 永久块模式**，本会话不重试 |
| 音频通道 | 依赖（Opus 编解码走 ffmpeg 子进程） | 见 §6.2；**目前 host 还没有采集/发送音频** |
| 验收脚本 | 依赖（`ffplay` 放测试画面） | 视频相关项会失败或被跳过 |

**两个通道的 ffmpeg 路径解析顺序现已一致**（`crates/lrc-video/src/ffmpeg.rs` 与 `crates/lrc-audio/src/proc.rs`）：

1. 代码里显式传入的路径（host/client 目前都传 `None`）
2. 环境变量 **`LRC_FFMPEG`**
3. 已知安装路径 `C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe`
4. `PATH` 里的 `ffmpeg`

> 音频通道（`lrc-audio`）的解析顺序与视频通道**完全一致**：设了 `LRC_FFMPEG` 两个通道都会用，不必分别配置。

### 1.3 构建与测试

```powershell
cd D:\lrc
cargo build --release              # 全部 crate（首次含 zstd-sys 编译，约 2 分钟）
cargo build --release -p lrc-host  # 只编被控端
cargo test --workspace             # 单元测试
```

产物：`target\release\lrc-host.exe`、`lrc-client.exe`、`lrc-bench.exe`，以及调试工具 `capture-probe.exe` / `cursor-probe.exe` / `render-probe.exe` / `lrc-video-probe.exe`。

---

## 2. 被控端 `lrc-host`

### 2.1 参数表

| 参数 | 默认 | 说明 |
|---|---|---|
| `--password <PASS>` | **必填**（或环境变量 `LRC_PASSWORD`） | 共享口令，即 HMAC-SHA256 的 key；命令行优先于环境变量 |
| `--listen <IP:PORT>` | `0.0.0.0:7800` | 监听地址。**默认监听所有网卡**（见 §5） |
| `--display <N>` | `0` | 采集第 N 个 DXGI 输出（0 起）。多屏时这是唯一选屏手段（见 §6.4） |
| `--fps <N>` | `30` | 块模式发送帧率上限，1..=240 |
| `--quality <N>` | `85` | 块模式 JPEG 质量 1..=100，越高越清晰、越大 |
| `--stats` | 关 | 每秒往 stdout 打一行统计（字段见 2.2）；`--stats=false` / `--no-stats` 可显式关 |
| `--video <auto\|off\|hevc\|h264>` | `auto` | 视频通道。`auto`/`hevc` **都只用 hevc_nvenc**（探测失败不会退 h264）；`off` 完全不探测 |
| `--video-bitrate <B>` | `4000000` | 视频码率 bit/s，100000..=80000000 |
| `--video-max-fps <N>` | `30` | 视频模式帧率上限 1..=240 |
| `--budget-bps <B>` | `0`（关闭） | 链路预算 bit/s。**>0 时启用自适应控制器**；0 = 参数固定，行为与不开完全一致 |
| `-h, --help` | | 打印帮助 |

> 没有 `--audio`：**host 目前还没有音频采集/发送**（`lrc-audio` 已实现采集/编码，尚未接进 host）。

示例：

```powershell
# 双屏，采集 DXGI 输出 1，开自适应、预算 3 Mbps
target\release\lrc-host.exe --listen 0.0.0.0:7800 --password 1234 --display 1 --fps 30 --video auto --budget-bps 3000000 --stats

# 只用块模式（没有 N 卡 / 不想起 ffmpeg 进程）
target\release\lrc-host.exe --password 1234 --video off --stats
```

### 2.2 `--stats` 每个字段的含义

每秒一行，样例：

```
t=12.0 fps=18.3 blocks=42 sent_frames=220 bytes=1234567 kbps=987.6 idle_frames=15 dropped=0 sec_bytes=12345 cursor_msgs=7 full_resends=1 mode=blocks video_kbps=0.0 video_fps=0.0 level=0 stage=原生 scale=1.00
```

| 字段 | 含义 |
|---|---|
| `t` | 会话内秒数 |
| `fps` | 这一秒真正发出的帧率 |
| `blocks` | 这一秒发出的变化块总数 |
| `sent_frames` | 会话累计发出的 `VideoFrame` 数 |
| `bytes` | 会话累计**线上字节**（payload + 4 字节长度前缀，含光标消息） |
| `kbps` | 这一秒的实际码率（kbit/s） |
| `idle_frames` | 这一秒采集到但**像素没变化**的次数（grab 返回 None 或候选块哈希全同）；静止时它涨、`kbps` 为 0 |
| `dropped` | 这一秒因流控（未确认字节超限）丢掉的帧数 |
| `sec_bytes` | 这一秒发出的字节数 |
| `cursor_msgs` | 这一秒发出的光标消息数（CursorPos + CursorShape；光标不进码流） |
| `full_resends` | 响应 `RequestFull` 的整帧重发次数（含静止桌面用缓存帧补的） |
| `mode` | `blocks` 或 `video` |
| `video_kbps` / `video_fps` | 视频模式的码率/帧率（块模式下为 0） |
| `level` / `stage` / `scale` | 自适应档位 0..9、阶段名（原生/降帧率/抬QP/降分辨率/近停发）、当前分辨率缩放；`--budget-bps=0` 时恒为 `0` / `原生` / `1.00` |

### 2.3 行为要点

- **单观看者**：一次只服务一条连接；客户端断开后回到 accept 等下一次连接。
- **首帧全量**：客户端连上后发 `RequestFull`，host 回一整帧（`flags = FULL|KEY`）。
- **流控**：未确认字节上限 `MAX_INFLIGHT_BYTES = 8 MiB`；收到过 `VideoAck` 才做窗口控制，一直没 ack 则退化为 TCP 背压 + 5 s 写超时。
- **视频通道切换**：变化块比例连续 3 帧 > 0.45 进视频模式；连续 30 帧 < 0.10 回块模式；**编码器任何一次失败 → 永久回块模式**。
- **自适应**（`--budget-bps > 0`）：控制器输出的 fps 不超过 `--fps`；质量/缩放/视频码率以控制器为准。
- **DPI**：启动即 `PER_MONITOR_AWARE_V2`，坐标全部按物理像素。

---

## 3. 主控端 `lrc-client`

### 3.1 参数表

支持 `--flag value` 与 `--flag=value` 两种写法。

| 参数 | 默认 | 说明 |
|---|---|---|
| `--connect <host:port>` | **必填** | 被控端地址，例如 `127.0.0.1:7800`；连不上会退避重试、窗口保持打开 |
| `--password <PWD>` | **必填** | 认证口令，HMAC-SHA256 的 key。**没有环境变量入口**（见 §5） |
| `--title <text>` | `lrc viewer` | 窗口标题 |
| `--stats` | 关 | 每秒打印一行统计（字段见 3.4） |
| `--audio on\|off` | `on` | 音频通道；设备不可用时自动降级关闭，不影响视频 |
| `--audio-dump <path>` | 无 | 把解码后的 PCM（s16le 48k/2ch）追加写文件，**调试用**（不轮转，文件会一直长） |
| `-h, --help` | | 打印帮助 |

窗口客户区默认 **1280×720**（代码里的默认值，不是命令行参数）；首帧到达后按远端宽高比自动调整一次。

### 3.2 窗口操作与输入映射

| 操作 | 行为 |
|---|---|
| 鼠标移动/按键/滚轮 | 换算成**被控端屏幕像素**后转发：`remote = client * remote_size / client_size`，越界会 clamp |
| 键盘 | 用 `lParam` 里的 **scancode**（bit16-23）+ extended 位转发，被控端以 `KEYEVENTF_SCANCODE` 回放，**与键盘布局无关** |
| 窗口大小 | 首帧（`flags FULL`）到达后按远端宽高比自动调一次客户区；之后随意拉伸，画面跟着缩放 |
| 光标 | 收到 `CursorShape` 后**本地合成**绘制（不再画系统光标），`CursorPos` 只更新位置 |
| `Ctrl+Alt+1..9` | 客户端会发 `SelectDisplay` 请求切换显示输出 —— **host 侧尚未实现**（见 §8 坑 2），当前等价于没反应 |

### 3.3 断线重连

- 连接失败 / 断线 / 握手的 IO 类失败 → **指数退避重连**：1、2、4、8、10、10… 秒（上限 10 s），窗口保持打开直到你关闭。
- **认证失败是终止性的**：密码错/版本不符只报一次错，不再重连（窗口保留）。
- 重连成功后重新发 `RequestFull` 要整帧，并清掉上一会话的脏块/视频帧状态（解码器随会话重建）。
- 音频线程**跨重连保留**（不反复开关设备）；`--audio off` 时不建线程。

### 3.4 `--stats` 每个字段的含义

每秒一行，样例：

```
fps=30 recv_kbps=1204.5 decoded_blocks=510 rtt_ms=12.300 mode=blocks video_fps=0.0 lat_ms=0.0 reconnects=0 audio=on audio_ms=60 audio_pkts=152 audio_drop=0 displays=0
```

| 字段 | 含义 |
|---|---|
| `fps` | 这一秒收到的帧率 |
| `recv_kbps` | 这一秒的接收码率（kbit/s） |
| `decoded_blocks` | 累计解码块数 |
| `rtt_ms` | `Ping/Pong` 测出的 RTT（毫秒） |
| `mode` | `blocks` 或 `video`（由 host 决定） |
| `video_fps` | 这一秒收到的视频帧率（块模式下 0） |
| `lat_ms` | 视频模式端到端延迟（两端同用 `GetTickCount64` 时钟时才有意义） |
| `reconnects` | 累计重连次数 |
| `audio` | `on` 正在播 / `off` 未启用或还没收到音频包 / `err` 设备或解码不可用（已关闭、不重试） |
| `audio_ms` | 播放器当前缓冲毫秒数 |
| `audio_pkts` / `audio_drop` | 累计音频包数 / 抖动缓冲溢出丢弃指标 |
| `displays` | 已知显示输出数（host 当前不发 `DisplayList`，通常为 0） |

---

## 4. 网络与带宽

### 4.1 端口与连接模型

- **默认端口 7800**（`DEFAULT_LISTEN = 0.0.0.0:7800`）。只需放行**一个 TCP 端口**。
- **单流点对点**：一条 TCP 连接、一个观看者；host 断开后才接受下一条连接。
- 传输层 = `u32` 长度前缀 + payload（`lrc-transport`），`TCP_NODELAY`，payload 上限 32 MiB。

### 4.2 实测带宽（见 `docs/MEASUREMENTS.md`，本机 1080p）

| 场景 | 实测 |
|---|---|
| 完全静止 | **0 B/s**（30 s 长跑共 370 B，全是 ack/ping） |
| 连续打字 | **≈ 2.1 KB/s = 16.8 kbps** |
| 高速滚动（全宽条带） | ≈ 300 KB/s = **2.4 Mbps** |
| 网页视频/照片流（块模式） | **6–50 Mbps** 波动 → 这就是要 `--video` 的原因 |
| 离线编码基准（`lrc-bench`） | 见 `crates/lrc-bench/BASELINE.md` |

### 4.3 `--budget-bps` 与 VPS 出口的关系

`--budget-bps` 是**整条上行链路的预算**（bit/s），不是某一路的配额。建议：

1. 先测真实上行（例如 VPS 出口 5 Mbps）；
2. 预算取实际上的 **60–80%**（给 TCP 重传、ack、突发留余量），例如 `--budget-bps 3000000`；
3. 开预算后 host 按固定 10 档降级：**降帧率 → 抬 QP → 降分辨率（1600×900 → 1280×720）→ 近停发（2 fps）**；
4. 滞回：连续 3 s 超预算降一档，连续 10 s 富余升一档，**丢帧当秒立刻降**并冷却 5 s；
5. 视频码率按档位缩放（预算的 0.70 → 0.25 倍），永远 ≤ 预算。

不开预算（默认）时所有参数固定，单机自测建议先不开。

---

## 5. 安全

**认证**：`ClientHello.nonce_c` + `ServerHello.nonce_s`，客户端回 `AuthRequest{ mac = HMAC-SHA256(key = 密码, msg = nonce_c || nonce_s) }`。密码本身**不上线**，每次连接 nonce 不同；失败原因见 `AuthResult.reason`（0 ok / 1 密码错 / 2 版本 / 3 限速）。

> ⚠ **传输层是明文 TCP，没有 E2EE**：画面、输入、音频都是裸的。只在内网或已加密隧道里用；跨公网请套 **WireGuard / SSH 端口转发 / stunnel**。

> ⚠ **默认 `--listen 0.0.0.0:7800` 监听所有网卡**（包括公网网卡）。建议至少三选一：
> - 只监听内网地址：`--listen 192.168.1.10:7800`；
> - 防火墙只放行单端口 7800 给可信来源；
> - 走 SSH 隧道（`ssh -L 7800:127.0.0.1:7800 user@host`），host 只监听 `127.0.0.1`。

口令没有限速/锁定机制，弱口令可被离线暴力尝试；请用长随机口令。

---

## 6. 排障

### 6.1 没有 NVENC / 视频通道起不来

- 现象：host 日志 `视频通道不可用（永久使用块模式）：...`，`--stats` 里 `mode` 永远是 `blocks`。
- `--video auto` 与 `--video hevc` **都只探测 `hevc_nvenc`**，失败**不会自动回退 h264**；想试 h264 必须显式 `--video h264`。
- 探测是**会话建立时真跑一次最短编码**，不是查注册表；以实际为准。
- 失败一次后**本会话内永久块模式**，不重试；重连或重启 host 才会重新探测。
- 完全没有 N 卡时用 `--video off`，省掉探测告警。

### 6.2 没有音频 / 没声音

- **host 目前没有音频采集与发送**（`lrc-audio` 已实现采集/编码，未接入 host），正常连接下 client 只会显示 `audio=off`。音频通路目前只被 `lrc-client/examples/fake-audio-host.rs` 验证。
- client 侧音频设备/解码器不可用 → 打一次告警，`audio=err`，**不重试、不影响视频**。
- 本机默认渲染设备是 `ToDesk Virtual Audio`（44100 Hz）；`lrc-audio` 内部做 44.1k⇄48k 线性重采样，设备无渲染端点或格式异常时报 `audio: 设备错误 ...`。
- 音频依赖 ffmpeg 的 Opus 编解码；路径规则见 §1.2，**与视频通道一致**（认 `LRC_FFMPEG`）。

### 6.3 HDR / 10bit 桌面

采集只接受 `DXGI_FORMAT_B8G8R8A8_UNORM(_SRGB)`；碰到 HDR（`R10G10B10A2`、`R16G16B16A16_FLOAT` 等）直接报错：

```
capture: 桌面纹理格式不是 B8G8R8A8_UNORM 类（HDR/10bit 桌面暂不支持，请切到 SDR）
```

处理：Windows 显示设置里**关闭 HDR**（或该显示器设为 SDR）后再连。

### 6.4 多屏与 DPI

- 选屏：`--display N` 是 **DXGI 输出序号（0 起）**，不是屏幕坐标顺序；先用 `capture-probe.exe --list` 看清序号再选。
- **`Ctrl+Alt+1..9` 切屏目前无效**：客户端会发 `SelectDisplay`，但 host 的消息分发只处理 `Input`/`VideoAck`/`RequestFull`/`Ping`，其余类型打 `忽略消息 type=...` debug 日志；host 也从不发 `DisplayList`。换屏只能改 `--display` 重启 host。
- DPI：两端都设 `PER_MONITOR_AWARE_V2`，全链路物理像素；客户区等比拉伸铺满，鼠标按同一公式反算，不会有 letterbox 偏移。

### 6.5 首帧黑屏 / 全零

DXGI 刚建好 Duplication 的第一帧经常是没填充的黑图（本机实测约 70% 全零）。`lrc-capture` 会抽样检测全零 → 丢弃重取 → 超过重试预算改用 **GDI `BitBlt` 兜底**画出当前桌面。重试预算可用环境变量 `LRC_BLANK_RETRY_MS` 调整。

### 6.6 卡顿 / 码率异常

- `kbps` 高且 `idle_frames` 也高 → 画面一直在动（视频类），考虑 `--video` 或 `--budget-bps`。
- `dropped` 一直涨 → 网络扛不住（或客户端 ack 太慢），开 `--budget-bps` 或降 `--fps`/`--quality`。
- `mode` 在 `blocks`/`video` 间反复跳 → 阈值 0.10–0.45 之间来回（静止+视频混排），属设计内行为；介意就 `--video off`。
- `--fps` 是上限不是保证：DXGI 只在画面变化时出帧，静止时本来就不发帧。

---

## 7. 验收脚本

全部在 `scripts\`，默认 `-HostDisplay 1`（**注意是第二块屏**，单屏机器要显式传 `-HostDisplay 0`）。

| 脚本 | 跑什么 | 看什么数字 |
|---|---|---|
| `acceptance.ps1` | **全量验收（M1+M2）**：一个会话里依次跑 静止 / 打字 / 视频 三阶段，输出 PASS/FAIL 表 | 每项 PASS；参数 `-HostDisplay` `-Password` `-Port` `-SkipBuild` `-SkipTests` |
| `verify-e2e-v2.ps1` | 真实 host + client，用 `PrintWindow` 抓客户区像素（不受遮挡影响），分离首帧与静止期 | 像素比对通过 + **静止期每秒线上字节数 ≈ 0**；参数 `-ObserveSeconds` |
| `measure-phase.ps1` | 单场景带宽：`-Mode static\|typing\|scroll`，预热 `-Warmup 5` 秒后只统计 `-Measure 10` 秒稳定窗口 | host 输出里 `sec_bytes`/`kbps` 的平均 |
| `verify-video.ps1` | 块模式 → ffplay 播整屏视频 → 应自动切视频模式 → 停播后应切回块模式 | 切换是否发生、视频码率是否落在 3–6 Mbps；参数 `-PlaySeconds`、`-VideoSource`（默认 `mandelbrot=s=1920x1080:r=30`） |

其它调试工具：

```powershell
target\release\capture-probe.exe --list                                # 列出 DXGI 输出序号
target\release\capture-probe.exe --display 1 --seconds 3 --timeout 0   # 采集层：idle 比例/耗时/dirty rect
target\release\cursor-probe.exe                                       # 光标形状/位置采集
target\release\render-probe.exe                                       # D3D11 呈现
target\release\lrc-video-probe.exe                                    # NVENC 编解码探测
cargo run --release -p lrc-bench -- --frames 120                      # 离线带宽基准
```

---

## 8. 已知限制与坑（读代码发现、其它文档没写的）

1. **host 没有音频**：`lrc-audio` 有 WASAPI 回环采集 + Opus 编解码 + 播放，但 `lrc-host` 既没有 `--audio` 参数也不发 `AudioFrame`；client 的音频路径只被 `examples/fake-audio-host.rs` 验证。
2. **`Ctrl+Alt+1..9` 切屏是空操作**：client 发 `SelectDisplay`，host 忽略未知消息；host 也从不发 `DisplayList`，所以 client `--stats` 的 `displays=` 基本恒为 0。
3. **`--video auto/hevc` 不回退 h264**：hevc_nvenc 探测失败即整条视频通道关闭。
4. **视频通道失败是永久性的**（本会话内），不会自动重试。
5. **`--budget-bps` 会覆盖 `--fps`/`--quality`/`--video-bitrate` 的固定值**（fps 不超过 `--fps`），开了预算就别指望固定画质。
6. **client 的密码只能走命令行**（无环境变量），会出现在 `tasklist`/进程命令行里；host 可用 `LRC_PASSWORD` 规避。
7. **`--audio-dump` 是调试口**：追加写、不轮转，长时间开机会撑大磁盘。
8. **音频在 client 是一次失败永久关闭**（`audio=err`），设备后来插上也不会自动恢复，要重启 client。
9. **host 单连接**：一个 host 进程同时只服务一个观看者，第二个客户端会一直连不上，直到第一个断开。
10. **HDR/10bit 桌面直接报错**，不会自动转 SDR。
11. **无损/有损是按块选的**：同一画面里文字块走无损、照片块走 JPEG，所以整屏无损不是目标；`--quality` 只影响走 JPEG 的块。
12. **`lrc-bench` 的数字依赖输入帧**：`captures/frame0.bgra` 换掉（或用全 0 占位帧）数字就不可比；bench 表头会打印 `xxh3_64` 指纹和合成帧兜底提示。
13. **`captures/*.bgra` 容易变成全 0 占位帧**（历史踩过两次）：bench 会自动识别并回退合成桌面；自己导出帧文件后建议看表头指纹确认非空。
14. **`--fps` 是上限不是保证**：真实帧率受采集和流控影响，静止时不发帧。
15. **验收脚本默认抓第 2 块屏**（`-HostDisplay 1`），单屏机器不传参就会失败。

---

## 9. 环境变量速查

| 变量 | 影响 |
|---|---|
| `LRC_PASSWORD` | host：口令（命令行 `--password` 优先） |
| `LRC_FFMPEG` | **视频与音频通道**共用的 ffmpeg 路径 |
| `LRC_VIDEO_DEBUG` | 存在即给视频通道 ffmpeg 打调试日志 |
| `LRC_VIDEO_NO_AUD` | 存在即给视频通道 ffmpeg 加 `-an` |
| `LRC_BLANK_RETRY_MS` | 采集：首帧全零的重试预算（毫秒） |
| `RUST_LOG` | 日志级别，默认 `info`；`debug` 能看到每帧/每条消息细节 |

---

## 10. 五分钟跑通（单机自测）

```powershell
cd D:\lrc
cargo build --release

# 1) 被控端：采集第 1 块屏，开统计
Start-Process target\release\lrc-host.exe -ArgumentList --password,1234,--display,0,--stats

# 2) 主控端：连本机
target\release\lrc-client.exe --connect 127.0.0.1:7800 --password 1234 --stats

# 3) 看两边 --stats：
#    host  ：静止时 kbps=0.0、idle_frames 增长；打字时 kbps 几十~几百
#    client：fps≈30、recv_kbps 跟随、mode=blocks
```

