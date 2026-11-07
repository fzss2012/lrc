# lrc-video 基线（NVENC 低延迟通道）

本机：RTX 3080 Ti / ffmpeg 8.0.1（gyan essentials build）/ 1080p。
所有数字是 `--release` 实测，不是估算。复现命令见文末。

---

## 1. 最终命令行（这些参数少一个就会崩，别删）

### 编码（BGRA → 裸 HEVC/H.264，活管道）

```
ffmpeg -hide_banner -loglevel error
  -f rawvideo -pix_fmt bgra -s WxH -framerate FPS -i pipe:0
  -c:v hevc_nvenc            # 或 h264_nvenc
  -preset p1 -tune ll
  -rc cbr -b:v BPS -maxrate BPS -bufsize BPS/FPS
  -g GOP -bf 0
  -zerolatency 1 -delay 0     # ← 关键，见坑 1
  -pix_fmt yuv420p
  -flush_packets 1 -fflags +nobuffer -flags +low_delay
  -f hevc pipe:1
```

### 解码（裸码流 → BGRA 原始帧）

```
ffmpeg -hide_banner -loglevel error
  -analyzeduration 0 -probesize 32   # ← 立刻开始解，别等探测缓冲
  -threads 1                          # ← 关键，见坑 2
  -f hevc -i pipe:0
  -f rawvideo -pix_fmt bgra pipe:1
```

解码端**不要**加 `-fflags +nobuffer`（坑 3），也不要 `-c:v hevc_cuvid`（坑 4）。

---

## 2. 五个坑（全部踩过、全部修好）

### 坑 1：nvenc 的 `-delay` 默认是 INT_MAX，输出晚 2 帧

`-zerolatency 1` 单独**不够**。表现：写进去 1~2 帧，管道里一点输出都没有；写到第 3 帧
才突然吐出第 1 帧的码流。必须显式 `-delay 0`，之后"写一帧 → 6~10ms 后拿到这一帧的 AU"。

### 坑 2：解码器必须 `-threads 1`

ffmpeg 默认对 h264/hevc 用**帧级多线程**，会先攒 N 帧（N=线程数）才吐画面。
在"喂一个 AU、同步等一张图"的用法里，表现就是**永远等不到输出**——
而**同一批字节存成文件再解又能正常出图**（因为文件有 EOF 会 flush），非常难定位。
这一个参数把 0 帧变成满帧。

### 坑 3：解码端加 `-fflags +nobuffer` 会丢掉第一个 AU

之后所有帧都报 `Could not find ref with POC N` / `Error constructing the frame RPS`。
A/B 实测：加了 0 帧，不加 1 帧。

### 坑 4：NVDEC（`-c:v hevc_cuvid`）不能用在同步逐 AU 模式

cuvid 内部同样攒帧，喂一个 AU 拿不到一张图。所以"解码 < 10ms"这条只能靠
**客户端异步管线**（喂进去、有帧就取，不等）来吃，见 §5。

### 坑 5：可用性探测不能用 64x64

NVENC 有最小分辨率限制（约 145x49），`color=black:s=64x64` 会被拒：
`InitializeEncoder failed: invalid param (8): Frame dimensions are less than the minimum supported value`。
探测必须用 ≥ 256x256，否则会把"能用的机器"误判成不可用。

---

## 3. 码流分帧（elementary stream 没有容器边界）

- 编码器零延迟参数（坑 1）+ `-flush_packets 1` ⇒ 写一帧就吐且只吐这一帧；
- 每次 `encode()` 写完把管道**抽干**：先阻塞等第一块数据，再读到"连续 idle_us（默认 600µs）
  没有新数据"为止 —— 这批字节就是这一帧的访问单元（AU）；
- 再用 Annex-B start code 扫 NAL：必须含 VCL NAL，并识别 IDR（HEVC 16..21 / H264 type 5）
  填 `last_au_was_keyframe()`；
- 解码侧在写 AU 之后再补一个 AUD NAL（HEVC `00 00 01 46 01 50`），让 ffmpeg 的 parser
  立刻把当前 AU 吐出来，不用等下一个 AU 的开始码。

---

## 4. 实测（1080p，60 帧/场景，e2e = 写帧→拿到解码帧）

`lrc-video-probe --frames 60 --bitrate 4000000 --codec hevc_nvenc --scenario all`

| 场景 | 目标码率 | 实际 | enc avg/p95 | dec avg/p95 | e2e p50/p95 | PSNR | dropped |
|---|---|---|---|---|---|---|---|
| static | 4 Mbps | 3.11 Mbps | 21.8/20.8 ms | 24.6/22.0 ms | 38.4/41.5 ms | **39.1 dB** | 0 |
| typing | 4 Mbps | 3.18 Mbps | 18.4/21.6 ms | 19.0/21.6 ms | 37.9/40.7 ms | **37.8 dB** | 0 |
| scroll（每帧全宽 4px） | 4 Mbps | 3.26 Mbps | 19.0/22.1 ms | 18.1/20.4 ms | 37.2/40.2 ms | 19.7 dB | 0 |
| video（桌面 + 40% 区域视频窗口） | 4 Mbps | 3.17 Mbps | 21.7/30.1 ms | 18.7/20.8 ms | 38.7/50.5 ms | **31.9 dB** | 0 |

其他实测：
- `probe("hevc_nvenc")` = available **true**，耗时 ~414 ms（含 ffmpeg 启动）
- `probe("h264_nvenc")` = available **true**，~413 ms
- `probe("hevc")`（软编）= true；`probe("h264")` = true（~88 ms）
- `set_bitrate(6 Mbps)` 重启耗时 **108.8 ms**，切档后第一帧是 IDR
- 进程残留：5~8 次起停后 `tasklist` 里 ffmpeg.exe 数量 **before=0 after=0**

### 验收对照

| 目标 | 实测 | 结论 |
|---|---|---|
| 1080p30 视频类 3–6 Mbps PSNR ≥ 32 dB | 31.9–39.1 dB（video 场景 31.9，边缘） | ✅（勉强） |
| 编码 < 20 ms | 18.4–21.8 ms | ⚠️ 边缘 |
| 解码 < 10 ms | 18–25 ms（CPU 单线程） | ❌ 未达标，需异步管线 + 硬件/多线程解码 |
| e2e p50 < 60 ms | 37.2–38.7 ms | ✅ |

---

## 5. 接主链路注意

1. **host 启动顺序**：`probe(codec)` → 不可用就整条视频通道关掉，退回纯块编码（M1 路径）。
2. **码率档位**：`BITRATE_LADDER = [3M, 4M, 6M]`，切换用 `set_bitrate()`，代价是重启进程
   ~109 ms（期间无输出，切档后第一帧 IDR，客户端可立刻解）。
3. **客户端解码**：`FfmpegDecoder` 是**同步**的（`decode(au) -> Option<frame>`），因为
   `-threads 1` 不攒帧所以能一进一出。想要更快（<10ms）必须改成异步管线 + 多线程/硬件解码，
   那时要接受 1~2 帧的流水线延迟。
4. **码流类型**：`lrc_video::codec_tag_of(codec)` → `lrc_proto::codec_tag::{HEVC, H264}`，
   直接填 `Message::VideoAu::codec`。
5. **进程清理**：`Drop`/`shutdown` = 关 stdin（让 ffmpeg 自己退）→ 等 1.5 s → 不死就 kill →
   join 读线程。构造到一半失败（编码器不可用）也在同一条路径上回收。

---

## 6. 复现命令

```powershell
cd D:\lrc
cargo build --release -p lrc-video
# 可用性探测
target\release\lrc-video-probe.exe --probe
# 全场景表
target\release\lrc-video-probe.exe --frames 60 --bitrate 4000000 --codec hevc_nvenc --scenario all
# 单元测试（含进程残留、set_bitrate、probe）
cargo test --release -p lrc-video
```

> 默认 ffmpeg 路径：`C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffmpeg.exe`；
> 可用环境变量 `LRC_FFMPEG` 或 `EncoderConfig.ffmpeg_path` 覆盖，否则退回 PATH。
> 调参环境变量：`LRC_VIDEO_IDLE_US`（分帧静默窗口，默认 600）。
