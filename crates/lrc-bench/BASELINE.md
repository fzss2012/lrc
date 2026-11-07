# lrc-bench 基线数字（M1 编码策略）

> 本文件是**回归基线**：改编码策略/参数后重跑 `cargo run --release -p lrc-bench -- --frames 120`，
> 与本表对比。数字口径见 `src/main.rs` 顶部注释。

## 环境

- Windows，Rust 1.93.1 MSVC，`--release`（root Cargo.toml：opt-level=3 + thin LTO + codegen-units=1）
- 帧：**真实桌面 1920x1080**，块 64x64 → 30x17 = 510 块，zstd level 3，JPEG q85，ratio_limit=0.55
- 帧来源说明：仓库里 `captures/frame0.bgra` 目前是**全 0 占位帧**（8,294,400 字节全 0），
  bench 会自动识别并回退到合成桌面。下面的真实内容数字是用 `captures/gdi-baseline.png`（3840x1080，
  真实抓屏）左半屏转出来的 BGRA 测的：
  ```python
  from PIL import Image
  im = Image.open(r'D:\lrc\captures\gdi-baseline.png').convert('RGBA').crop((0, 0, 1920, 1080))
  ba = bytearray(im.tobytes()); ba[0::4], ba[2::4] = ba[2::4], ba[0::4]
  open(r'D:\lrc\captures\frame0.bgra', 'wb').write(bytes(ba))
  ```

## 主表（真实桌面内容，120 帧/场景）

```
scenario   changed_blocks  bytes_avg bytes_p95 bytes_max    kbps@30fps enc_ms_avg  tag[L/J/R]
static                0.0        0.0         0         0          0.00      0.900  L0/J0/R0
typing               16.1     4245.0      7297      7865       1018.80      1.756  L1936/J0/R0
scroll               58.1    27665.6     37671     40546       6639.75      4.526  L6967/J0/R0
full                298.5   119998.4    122693    123276      28799.61     16.501  L35819/J0/R0
video               510.0  1313122.9   1543809   1609446     315149.48    173.937  L13316/J47884/R0
```

- **static = 0 字节**：零变化零发送成立（连帧头都不发）。
- typing ≈ **1.02 Mbps**，scroll ≈ **6.64 Mbps**，full ≈ **28.8 Mbps**，video ≈ **315 Mbps**。
- enc_ms_avg = 哈希差分 + encode_block 的单帧耗时；full 16.5ms，video 174ms（单线程 1080p 全帧 JPEG 只有 ~5.7fps）。

## 策略对比（同一批脏块，B/帧均值）

```
strategy               static     typing     scroll       full      video        total
all-lossless              0.0     4245.0    27665.6   119998.4  4819481.4    4971390.4
all-jpeg-q85              0.0    23075.0    81084.9   376586.5   481805.9     962552.2
mixed (policy)            0.0     4245.0    27665.6   119998.4  1313122.9    1465031.8
mixed / all-jpeg            -       5.44x       2.93x       3.14x       0.37x         0.66x
mixed / all-loss            -       1.00x       1.00x       1.00x       3.67x         3.39x
```

办公/滚动/满帧：混合策略 = 全无损，比全 JPEG 省 2.9~5.4 倍。
video：混合反而比全 JPEG **差 2.7 倍** —— `lossless_ratio_limit=0.55` 在平滑图像块上
过早接受了无损（无损 ~50% 就放行，而 JPEG 只要 ~6%）。

## ratio_limit 敏感性（真实帧，30 帧，--no-verify）

| ratio_limit | typing | scroll | full | video |
|---|---|---|---|---|
| 0.55（当前默认） | 4303.5 | 25908.0 | 120001.2 | **1338959.7** |
| 0.20 | 4303.5 | 25908.0 | 120001.2 | **481376.9**（2.78x 更小） |

→ 建议把 `lrc_codec::Options::default().lossless_ratio_limit` 从 0.55 调到 ~0.20：
办公场景字节**逐字节不变**（仍走无损），视频类内容省 2.78 倍。

## 合成帧基线（占位帧存在时的默认路径）

`cargo run --release -p lrc-bench -- --frames 120` 在 `captures/frame0.bgra` 仍是占位帧时，
会用合成桌面（同尺寸，窗口 + 文字），数字仅用于回归管线行为：

```
scenario   changed_blocks  bytes_avg bytes_p95 bytes_max    kbps@30fps enc_ms_avg  tag[L/J/R]
static                0.0        0.0         0         0          0.00      0.889  L0/J0/R0
typing               16.1     6288.9      9640      9804       1509.35      2.076  L1936/J0/R0
scroll              117.5    38241.3     44965     45035       9177.92      7.441  L14096/J0/R0
full                510.0   146621.8    148238    148580      35189.23     23.006  L61200/J0/R0
video               510.0  1313122.9   1543809   1609446      315149.48    171.008  L13316/J47884/R0
```

## 怎么跑

```bash
cargo run --release -p lrc-bench -- --frames 120            # 全场景，约 60s
cargo run --release -p lrc-bench -- --frames 120 --no-verify # 跳过解码校验，约 45s
cargo run --release -p lrc-bench -- --scenario typing,scroll --frames 120
cargo run --release -p lrc-bench -- --ratio-limit 0.20 --frames 30
cargo test --release -p lrc-bench                            # 场景/口径/复刻一致性共 11 个测试
```

## 已知限制

- `lrc-bench` 只测**编码侧**：不含采集拷贝、渲染、网络抖动/重传。
- 「全无损 / 全 JPEG」两行是 bench 内复刻 codec 容器的旁路实现，运行时会与 `lrc-codec`
  的真实输出逐字节交叉校验（不一致会在输出里告警）；codec 改格式后记得先看这条。
- scroll 场景的条带内容取自真实帧另一段（不是自造文字），所以脏块数取决于条带里实际有多少细节。
