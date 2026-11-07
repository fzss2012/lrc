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
u8  mode          // 0 = BGRA u32 左预测后 zstd；1 = 原样 zstd（压缩效果更好时）
u16 w  u16 h
payload           // zstd(level 3)
```
预测器：每行首像素原样，其后 `d[i] = (p[i] - p[i-1]) as u8`（按通道环绕减法）。
理由：桌面文字/UI 大面积同色，左预测后差值大量为 0，zstd 极易压。

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
