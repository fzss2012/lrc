# lrc 设计（M1 基线）

## 0. 原则

- **单受众点对点**：不做 simulcast/SVC/SFU/多租户。一路流、一个观看者、一个 TCP/QUIC 连接。
- **传输层只管字节**：M1 用 TCP + 长度前缀，消息语义全部在 `lrc-proto`。
- **零变化零发送**：采集侧没有变化 → 上限时器唤醒但不发任何帧（连帧头都不发）。
- **块是第一公民**：脏区最终被规约到 64×64 块网格（16 的倍数可调），编码、哈希、增量都以块为单位。

## 1. 数据流

```
被控端 lrc-host
  DXGI AcquireNextFrame(→dirty rects/move rects)
    → 拷到 staging BGRA 缓冲
    → 64×64 块 xxh3_64 哈希，与上一帧对比
    → 变化块：无损(zstd+左预测) 与 JPEG 各试一次，取小
    → 打包 VideoFrame(seq, grid, entries[])
    → lrc-transport 发送（按 client ack 做滑动窗口/字节上限）

主控端 lrc-client
  收 VideoFrame → 逐块解码 → blit 到 D3D11 纹理 → Present
  本地鼠标/键盘 → InputEvent → 回发
  光标位置独立消息 → 本地合成（不进码流）
```

## 2. 线格式（v1，小端）

传输层：每条消息 = `u32 payload_len`（小端，不含自身）+ payload。
payload 首字节 = msg_type。

| type | 方向 | 名称 |
|---|---|---|
| 0x01 | C→H | ClientHello |
| 0x02 | H→C | ServerHello |
| 0x03 | C→H | AuthRequest |
| 0x04 | H→C | AuthResult |
| 0x10 | H→C | VideoFrame |
| 0x11 | H→C | CursorShape |
| 0x12 | H→C | CursorPos |
| 0x13 | C→H | VideoFrameAck |
| 0x20 | C→H | InputEvent |
| 0x30 | C→H | RequestFullFrame |
| 0x40 | C→H | Ping |
| 0x41 | H→C | Pong |

### 2.1 握手

```
ClientHello { version:u16, client_id:[u8;16], nonce_c:[u8;16] }
ServerHello { version:u16, width:u32, height:u32, rotation:u16, nonce_s:[u8;16] }
AuthRequest { mac:[u8;32] }            // HMAC-SHA256(key=password, msg = nonce_c || nonce_s)
AuthResult  { ok:u8, reason:u8 }       // reason: 0 ok, 1 bad password, 2 version, 3 rate
```

### 2.2 VideoFrame

```
seq:u32  flags:u8  base_seq:u32
block_size:u16  blocks_x:u16  blocks_y:u16
count:u16
[ count × { idx:u32, codec:u8, len:u32, data[len] } ]
```

flags: bit0 = full（解码端需先清屏/期待全网格）, bit1 = keyframe。
codec: 0 = Lossless(zstd+left-predict), 1 = Jpeg, 2 = Raw BGRA。

`idx = y * blocks_x + x`；块像素矩形 = `(x*block_size, y*block_size, min(block_size, W-x*bs), min(block_size, H-y*bs))`。

### 2.3 InputEvent

**坐标语义（钉死）**：`InputEvent` 里的 `x/y` 是**被采集那一路输出（display）的本地像素坐标**，
左上角为该输出的 (0,0)。被控端注入时自己加上 `ScreenInfo.origin_x/origin_y` 得到虚拟桌面坐标，
再按 `SM_XVIRTUALSCREEN/SM_CXVIRTUALSCREEN` 归一化成 0..65535 交给 `SendInput`。
这样主控端只管"我看到的那块画面里的第几个像素"，不用关心多屏布局。

### 2.3.1 InputEvent

```
kind:u8
  0 = MouseMove { x:i32, y:i32 }            // 被控端屏幕像素坐标
  1 = MouseButton { button:u8, down:u8, x:i32, y:i32 }
  2 = Wheel { dx:i16, dy:i16, x:i32, y:i32 }
  3 = Key { scancode:u16, down:u8, extended:u8 }
  4 = KeyUnicode { codepoint:u32, down:u8 }  // 输入法兜底
```

### 2.4 光标

`CursorPos { x:i16, y:i16, visible:u8 }`
`CursorShape { width:u16, height:u16, hotspot_x:i16, hotspot_y:i16, rgba:bytes }`（长度 = 4*w*h，空表示隐藏）

## 3. 块编码

### 3.1 无损（codec=0）

```
u8  mode          // 0 = BGRA u32 左预测后 zstd
                  // 1 = 原样 zstd
                  // 2 = 调色板 8bit（≤256 色）：u16 n + n*4 调色板 + 每像素 1 字节索引，整体再 zstd
                  // 3 = 调色板 4bit（≤16 色）： u16 n + n*4 调色板 + 每像素 4bit 索引，整体再 zstd
u16 w  u16 h
payload
```
预测器：每行首像素原样，其后 `d[i] = (p[i] - p[i-1]) as u8`（按通道环绕减法）。
理由：桌面文字/UI 大面积同色，左预测后差值大量为 0，zstd 极易压。

调色板模式（mode 2/3）对"少量颜色 + 锐利边缘"的文字/UI 最有效：实测办公内容里
**90–98% 的无损块会命中调色板**，合成文字块 355 B → 242 B。
编码端四种候选（0/1/2/3）都算一遍取最小；解码端按 mode 分支，旧数据（0/1）仍可解。

### 3.2 有损（codec=1）

```
u8 quality  u16 w  u16 h
payload           // 基线 JPEG（4:4:4 优先，文字网格更清晰）
```
M1 用纯 Rust jpeg 编码器；M2 换 H.264/HEVC（NVENC 硬编）+ 流式视频通道。

### 3.3 选择策略

每变化块：先试无损 → 若 `lossless_len * 4 < raw_len` 直接采用；
否则试 JPEG，取较小者。都明显大于 raw 则 raw。（阈值可在 `--quality` 档位调整。）

## 4. 采集

- DXGI `IDXGIOutputDuplication`，`AcquireNextFrame(0)` 非阻塞轮询（首次 500 ms 阻塞）。
- `GetFrameDirtyRects` + `GetFrameMoveRects`；dirty 面积 > 60% 全屏或
  `AccumulatedFrames > 4` 时视为整帧。
- 无新帧（超时）→ 直接返回 `None`，不产生任何消息。
- 兜底：GDI `BitBlt`（老系统/远程桌面嵌套）。
- **首帧有效性**：`AcquiredFrame.AccumulatedFrames == 0` 的那一帧里没有桌面图像（拷出来全黑）。
  非首帧直接丢弃；首帧做抽样检查 + 有界重试，重试用尽改用 GDI BitBlt 兜底，避免客户端黑屏。
- **格式校验**：桌面纹理必须是 `B8G8R8A8_UNORM`/`_SRGB`，HDR/10bit 直接返回 `Unsupported`。

### 4.1 取帧 API（M2 起推荐 `grab_view`）

```rust
// 旧接口：拥有数据，每帧一次 8.1 MB 全屏拷贝（1080p30 ≈ 243 MB/s）
let frame: Frame = cap.grab()?.unwrap();
use_frame(&frame.bgra, frame.stride, &frame.dirty, frame.full);

// 新接口（推荐）：零拷贝借用，生命周期到下一次 grab_view 为止
if let Some(v) = cap.grab_view()? {
    // v.width / v.height / v.stride / v.bgra: &[u8] / v.dirty: &[Rect] / v.moves / v.full / v.ts_ms
    encode_blocks(v.bgra, v.stride, v.dirty);
}   // ← 这里之后才能再 grab_view
```

两者语义完全一致（同一份像素、同一组 dirty/moves），唯一区别是**谁拥有内存**：

| | 单帧 p50 | 每帧内存拷贝 | 适用 |
|---|---|---|---|
| `grab()` | ≈ 3.8–4.0 ms | 8.1 MB | 需要跨线程/缓存帧 |
| `grab_view()` | ≈ 1.4–1.9 ms | **0** | host 主循环（取一帧→编码→下一帧） |

host 迁移方式：把循环体里的 `let frame = cap.grab()?...` 换成 `let Some(v) = cap.grab_view()?...`，
`frame.bgra` → `v.bgra`、`frame.stride` → `v.stride`、`frame.dirty` → `v.dirty`，其余不变
（注意 `v` 借用 `cap`，不能在持有时再调 `cap` 的方法）。

## 4.5 画面缩放口径（钉死）

**Presenter 用 `DXGI_SCALING_STRETCH` 把被控端画面铺满主控端客户区**（不 letterbox），
因此主控端的鼠标映射必须用**等比拉伸**而不是 letterbox：

```
remote_x = client_x * remote_w / client_w     (再 clamp 到 [0, remote_w-1])
remote_y = client_y * remote_h / client_h
```

另外主控端在收到首帧（flags FULL）后，**把窗口客户区调成被控端的宽高比**（保持当前宽度、按比例算高度），
这样拉伸失真在常规使用下不存在。非 16:9 被控端（如 1920×1200）也不会点不准。

## 4.6 M2：视频通道（设计草案，接口待定稿）

**问题**：块内 JPEG 对"视频/照片/整屏重绘"内容必然爆带宽（实测 6–50 Mbps、CPU 174 ms/帧）。

**方案**：双模式 + 自动切换。

```
每帧统计变化块数与变化面积比
  ├─ 变化块 < 25% 且面积 < 40%  → 块模式（当前路径，文字无损最划算）
  └─ 连续 K 帧（K=3）超过阈值   → 视频模式：
       整帧 BGRA → NVENC（HEVC 优先，回落 H.264）
       → 线格式 Message::VideoAu { ts_ms, seq, codec, key, data }
       → 客户端 ffmpeg/解码器 → BGRA → 直接 update_full
回落到块模式：连续 M 帧（M=30）变化块 < 5% 且面积 < 10%
```

要点：
- **视频模式只在"确实是视频类内容"时打开**，否则文字会被有损编码（这是 ToDesk 用 COMPOUND 逐块分割解决的问题；我们先做帧级开关，够用）。
- 视频模式期间 **光标照旧走独立消息**（不进码流）。
- 码率由简单的目标值 + 帧率阶梯控制（M2 先不做完整 GCC）：`video_bitrate` 由 `--quality` 档位映射，帧率 30/20/15。
- 关键帧只在建链/切换模式时请求（`-g 60` + `-bf 0` + low-delay）。
- 客户端解码失败/丢包 → 发 `RequestFull`，host 请求 IDR。

**线格式新增（proto v1 向后兼容，新增消息类型 0x14）**：

```
VideoAu { ts_ms:u32, seq:u32, codec:u8, flags:u8, data:blob }
codec: 3 = H264, 4 = HEVC
flags: bit0 = key
```

## 5. 输入

- 被控端 `SendInput`：鼠标走 `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE` 以
  65535 归一化到虚拟桌面；键盘走 `KEYEVENTF_SCANCODE`（布局无关）。
- 主控端捕获：窗口消息（M1）→ `WM_INPUT` raw input（M1.5，手感更准）。
- **DPI**：两端都 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，
  坐标一律用物理像素，避免"点不准"。

## 6. M1 验收

| 项 | 标准 |
|---|---|
| 局域网 1080p 办公 | 打字/滚动/拖窗口可用，延迟 < 60 ms |
| 静止 | `stats` 显示 1s 内 sent_bytes = 0 |
| 码率 | 办公 ≤ 1.0 Mbps，滚动 ≤ 3 Mbps |
| 正确性 | 逐块解码后像素与源一致（无损块 md5 相同） |
