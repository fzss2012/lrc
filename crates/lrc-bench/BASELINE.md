# lrc-bench 基线数字（M1 编码策略）

> 回归基线：改编码策略/参数后重跑 `cargo run --release -p lrc-bench -- --frames 120`，与本表对比。
> **换帧之后数字不可直接比**——bench 会在表头打印帧指纹 `xxh3_64`。

## 版本

- **T10**：`lossless_ratio_limit` 0.55 → 0.20；无损新增调色板模式（mode2 = palette8 / mode3 = palette4）；
  左预测改 SWAR（16KB 块 25.3us → 1.8us）；`lrc-codec` 新增两个对照入口
  `encode_lossless_block` / `encode_jpeg_block`（bench 不再自己复刻容器格式）。
- 机器：Windows / Rust 1.93.1 MSVC / `--release`（opt-level=3 + thin LTO + codegen-units=1）。

## 本基线的输入帧

| 项 | 值 |
|---|---|
| 文件 | `captures/frame0.bgra`（1920x1080 BGRA 紧排，8,294,400 B） |
| 帧指纹 | `xxh3_64 = 01a1f622ab5bacaa` |
| 内容 | **x.com 信息流页面（含视频/照片）**，块内平均唯一色 **241** —— 偏"图像"，不是典型办公画面 |

> 想要"办公/文字"基线，抓一帧 IDE / 终端 / 文档画面再跑；纯文字内容块内颜色数只有十几，
> 无损和调色板都会明显更省（见文末"内容相关性"）。

## 主表（120 帧/场景，ratio_limit=0.20，含调色板）

```
scenario   changed_blocks  bytes_avg bytes_p95 bytes_max    kbps@30fps enc_ms_avg  tag[L/J/R]
static                0.0        0.0         0         0          0.00      0.958  L0/J0/R0
typing               16.1     9013.3     15167     16987       2163.20      2.591  L1753/J183/R0
scroll               64.5    50318.2     60550     64494      12076.36      9.625  L5965/J1772/R0
full                313.1   240101.5    250112    260937      57624.35     33.047  L29664/J7913/R0
video               510.0   481805.9    483074    483633     115633.41    184.257  L0/J61200/R0
```

- **static = 0 字节**：零变化零发送成立。
- 编码耗时含"哈希差分 + encode_block"，不含解码校验。
- 解码校验：108,450 个块全部解回，LOSSLESS/RAW 逐字节一致，失败 0。

## 无损模式分布（palette 命中率）

```
scenario     lossless     mode0 预测     mode1 原样    mode2 pal8    mode3 pal4   palette 占比
static              0            0            0             0             0         0.0%
typing           1753          115          452           364           822        67.7%
scroll           5965          334         3311          1551           769        38.9%
full            29664         1576        16270          7444          4374        39.8%
video               0            0            0             0             0         0.0%
```

全程无损块 37,382 个，其中 **15,324 个（41.0%）走调色板**（pal8 9,359 / pal4 5,965）。

## 策略对比（同一批脏块，B/帧均值）

```
strategy               static     typing     scroll       full      video        total
all-lossless              0.0    15659.3   138152.6   597478.3  4819481.4    5570771.7
all-jpeg-q85              0.0    25040.5    83017.1   391368.4   481805.9     981231.9
mixed (policy)            0.0     9013.3    50318.2   240101.5   481805.9     781238.8
mixed / all-jpeg            -       2.78x       1.65x       1.63x       1.00x         1.26x
mixed / all-loss            -       1.74x       2.75x       2.49x      10.00x         7.13x
```

## ratio_limit 0.55 → 0.20（同一帧 A/B，30 帧，两次都开调色板）

| scenario | 0.55（旧） | 0.20（新） | 变化 |
|---|---|---|---|
| static | 0.0 | 0.0 | — |
| typing | 10,974.6 | 7,675.7 | **-30.1%** |
| scroll | 81,684.9 | 44,838.9 | **-45.1%** |
| full | 486,383.8 | 240,487.6 | **-50.6%** |
| video | 1,338,959.7 | 481,376.9 | **-64.0%** |

收紧阈值只会把"无损已经不太划算"的块改送 JPEG（每块取 min），所以字节单调不增。
本帧是照片/视频多的画面，收益比纯办公画面大。

## 调色板效果（与"左预测 + 原样 zstd 取小"对比，逐块无损候选字节）

| 内容 | 平均唯一色/块 | 旧无损候选 | 加调色板 | 省 | 走调色板的块 |
|---|---|---|---|---|---|
| 合成文字块 64x64（3 色） | 3 | 355 B | **242 B** | **-31.8%** | 1/1 |
| 旧"文字桌面"帧（IDE+对话框） | 16 | 123,607 B | 108,988 B | **-11.8%** | 190/510 |
| 本基线帧（x.com 网页/照片） | 241 | 605,651 B | 599,781 B | **-1.0%** | 100/510 |

结论：调色板吃"同色块多"的内容（文字/图标/纯色面板）；照片类块大多 >256 色，
或者调色板 + 索引流打不过 zstd 对原始 BGRA 的 LZ 匹配，命中率和收益都低。

## 单块耗时（release，3 次取中位，64x64）

| 路径 | encode | decode |
|---|---|---|
| 文字块（调色板命中） | **46.5 us** | **11.1 us** |
| 同块"旧两模式路径"（无调色板，仅对照） | 27.6 us | — |
| 照片/噪声块（走 JPEG） | 257~279 us | 123 us |

- 调色板本身约 +19 us/块（1.69x），但 SWAR 左预测把 16KB 预测从 25.3 us 降到 1.8 us，
  所以文字块**总耗时仍比 T5 版本的 ~53 us 略快**。
- 照片块被 `best*4 > raw` 的门槛挡住，不做调色板，耗时和 T5 持平。

## 相对 T5 旧基线（**不同帧**，仅供参考，不要直接当回归结论）

| scenario | T5 旧基线（文字桌面帧） | 现在（x.com 照片帧） |
|---|---|---|
| static | 0.0 | 0.0 |
| typing | 4,245.0 | 9,013.3 |
| scroll | 27,665.6 | 50,318.2 |
| full | 119,998.4 | 240,101.5 |
| video | 1,313,122.9 | 481,805.9 |

差异主要来自**输入帧内容**（旧帧块内平均 16 色，新帧 241 色）；同一帧上的真实对比见上面
"ratio_limit A/B" 与"调色板效果"两节。

## 怎么跑

```bash
cargo run --release -p lrc-bench -- --frames 120             # 全场景，约 65s
cargo run --release -p lrc-bench -- --frames 120 --no-verify  # 跳过解码校验
cargo run --release -p lrc-bench -- --scenario typing,scroll --frames 120
cargo run --release -p lrc-bench -- --ratio-limit 0.55 --frames 30   # 老策略对照
cargo test --release -p lrc-codec -p lrc-bench                # 15 + 12 个测试
```

## 已知限制 / 踩过的坑

- 只测编码侧：不含采集拷贝、渲染、网络抖动/重传。
- **回归坑**：T10 的调色板最初用"左预测差分是否为 0"判断"和左邻同色"，
  但逐字节预测里 `[0xFF,0xFF,0xFF,0xFF]` 这种像素差分恒为 0（左邻 alpha 也是 0xFF 时），
  导致白像素被编码成左邻颜色。是 bench 的逐块解码校验抓到的（130/510 块不一致），
  已修 + 加了回归测试（codec 的 `palette_handles_all_equal_byte_pixels`、
  bench 的 `real_frame_lossless_roundtrip`）。**别删这两条测试。**
- 「全无损 / 全 JPEG」两行现在直接调 `lrc-codec` 的对照入口，和混合策略同一份实现，
  不会再出现"bench 复刻格式和 codec 不同步"的问题。
