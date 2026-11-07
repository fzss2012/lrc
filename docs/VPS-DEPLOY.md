# lrc 项目总结 + 新 VPS 部署手册（Ubuntu 24.04）

> 面向：重装一台干净的 Ubuntu 24.04，把 `lrc-relay`（P2P 信令 + UDP 中继）跑起来，
> 然后让 Windows 端的 `lrc-host` / `lrc-client` 通过它做跨网 P2P 测试。

---

## 一、项目现状总结

**代码位置：`D:\lrc`**（独立 git 仓库，13 个 crate / 25,980 行 Rust / 240 个单测）。

### 1.1 已经做完并验证过的

| 目标 | 实现 | 实测证据 |
|---|---|---|
| DXGI 采集 + 零变化零发送 | `lrc-capture` | 静止桌面 **0 B/s**；30 秒长跑仅 370 B；采集层 99.9% 的 `grab()` 返回 None |
| 块级脏区（64×64） | `lrc-capture` + `lrc-host` | 办公打字只发 1–2 块；按脏区限制哈希（全屏 510 块 → 通常 1–20 块） |
| 文字无损 / 图像有损逐块分流 | `lrc-codec` | 文字块无损 **355 B** vs JPEG **3,099 B**（省 8.7×）；办公内容 90–98% 命中调色板 |
| 可靠传输 | `lrc-transport` + `lrc-udp` | TCP 分帧 + 可靠有序 UDP（分片/ACK/NACK/RTO/保活），10% 丢包仍全量有序 |
| 输入注入回放 | `lrc-host` | SendInput，双屏 origin 换算实测点对屏 |
| 光标独立通道 | `lrc-capture` + `lrc-client` | 形状+位置+本地合成，移动后旧位置无残影 |
| 硬编 H.264/HEVC | `lrc-video`（NVENC via ffmpeg） | 视频内容 **29–41 Mbps → 0.7–3.1 Mbps（≈10×）**，PSNR 32–33 dB |
| 自适应降级 | `lrc-adapt` | 10 档阶梯 + 滞回；2 Mbps 预算下 3 秒收敛且不震荡 |
| 音频 | `lrc-audio` | WASAPI 回环 → Opus，**38 kbps**、静音 0 字节、抖动缓冲 50–60 ms |
| 多显示器切换 | proto + host/client | Ctrl+Alt+数字，切完回整帧重同步 |
| **P2P / relay** | `lrc-relay` + `lrc-udp` | **本机实测通过**（见 §1.2） |
| 断线自动重连 | `lrc-client` | 指数退避 1/2/4/8/10s；杀 host 后自动恢复画面 |

### 1.2 关键实测数字（本机 loopback，非跨公网）

```
scripts/acceptance.ps1        → 7/7 全通过（含 240 个单测）
  静止零发送        min = 0 B/s
  办公打字          avg = 170–373 kbps
  视频模式          mode=video，video_kbps 0.7–3.1 Mbps
  视频管线延迟      中位数 33 ms（p90 41 ms）
  停播自动回块模式   ✓

scripts/verify-p2p.ps1        → 2/2 全通过
  --p2p auto   两端 path=direct_udp，relay 转发 0 字节（媒体完全不经过 relay）
  --p2p relay  两端 path=relay_udp，relay 正常转发 1.85–2.6 MB

scripts/verify-p2p-lossy.ps1  → 通过
  loss 0 / 5 / 10%：会话不中断，码率最差 1.06×，udp_loss 0/46/70‰
  全丢 14s：客户端按保活超时判死 → 重连 1 次 → 恢复 30 fps
  （大帧拆 ≤32KB 后，5% 丢包下首屏 5s → 1s）
```

> ⚠️ **重要**：以上 P2P 全部是**本机 loopback + 本机 relay** 验证的。
> **跨公网 / 跨 NAT 的真实打洞尚未验证过**（这正是本次要在新 VPS 上做的事）。

### 1.3 文档索引

| 文件 | 内容 |
|---|---|
| [docs/USAGE.md](USAGE.md) | 完整使用手册（参数、stats 字段、网络、安全、排障 15 条） |
| [docs/DESIGN.md](DESIGN.md) | 线格式、块编码、采集、输入、M2 视频、M4 传输层设计 |
| [docs/MEASUREMENTS.md](MEASUREMENTS.md) | 全部实测数据 |
| [docs/ROADMAP.md](ROADMAP.md) | M1–M4 里程碑与完成状态 |
| 本文件 | 项目总结 + VPS 部署手册 |

---

## 二、新 VPS 部署 `lrc-relay`（干净 Ubuntu 24.04）

### 2.0 先决条件：**安全组必须放行 UDP 7801**

> **已确证（2026-11-07 重装后实测）**：这台腾讯云机器**入站 UDP 全部不通**，TCP 正常。
> 证据：在 VPS 上用 `tcpdump` 抓包 + Python 监听多端口，从 Windows 各打 3–8 个 UDP 包到 **7801 / 3478 / 45000，三个端口全部 0 个到达**；
> 同一时刻 **TCP 7800 从 Windows 连接成功**；VPS 内 `ufw inactive`、`iptables INPUT ACCEPT` 说明不是本机防火墙。
> → 结论：**安全组里一条 UDP 入站规则都没有**，必须手动加。

这是本次唯一没走通的一环，**务必先做**：

- 腾讯云控制台 → 云服务器/轻量 → 防火墙/安全组 → **入站规则** → 添加：
  - 协议 **UDP**，端口 **7801**，来源 `0.0.0.0/0`
- 如果你还想保留 TCP 直连兜底，另开 **TCP 7800**（可选，被控端监听用）
- 云主机系统内如果开了 `ufw`，也要 `sudo ufw allow 7801/udp`

验证方法（VPS 上）：

```bash
sudo tcpdump -n -U -i any udp port 7801     # -U 不缓冲，别漏这参数
# 另开一个 Windows 终端打几个包（见 §2.7），tcpdump 要有输出
```

### 2.1 登录

镜像是 Ubuntu 24.04，**默认用户是 `ubuntu`，不是 root**（本次实测 `root@` 被拒）：

```powershell
ssh -i C:\Users\sss\.ssh\key117.pem ubuntu@<VPS_IP>
```

> **Windows 端坑**：如果报 `UNPROTECTED PRIVATE KEY FILE / bad permissions`，
> 先收紧 ACL（我已在本机修过一次，换机器要重做）：
> ```powershell
> $key = 'C:\Users\sss\.ssh\key117.pem'
> icacls $key /inheritance:r
> icacls $key /grant:r "$($env:USERNAME):R"
> ```

### 2.2 安装 Rust（用国内镜像，直连会卡）

**直连 crates.io 在腾讯云上实测卡死 20 分钟**，必须走镜像：

```bash
# rustup 本体走中科大镜像
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh
sh /tmp/rustup.sh -y --profile minimal --default-toolchain stable --no-modify-path
source "$HOME/.cargo/env"
rustc --version && cargo --version      # 本次装到 1.99.1

# crates 依赖走 rsproxy 镜像（仓库里有现成脚本 scripts/deploy/setup-cargo-mirror.sh）
mkdir -p ~/.cargo
cat > ~/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
retry = 3
EOF
```

### 2.3 上传源码（只传 relay 需要的最小集合）

在 **Windows** 上打包（`lrc-relay` 只依赖 `lrc-proto` + `lrc-udp`，不用传 host/client）：

```powershell
powershell -NoProfile -File D:\lrc\scripts\deploy\package-relay-src.ps1
# 产出 D:\lrc\.deploy\relay-src.tar.gz（约 50 KB）

scp -i C:\Users\sss\.ssh\key117.pem D:\lrc\.deploy\relay-src.tar.gz ubuntu@<VPS_IP>:/tmp/
ssh -i C:\Users\sss\.ssh\key117.pem ubuntu@<VPS_IP> "mkdir -p ~/lrc && tar -xzf /tmp/relay-src.tar.gz -C ~/lrc"
```

### 2.4 构建

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd ~/lrc/relay-src
cargo build --release -p lrc-relay
# 本次实测：Finished release in 29.17s，产物 9,384,544 字节
ls -la target/release/lrc-relay
```

> 注意：`lrc-udp` 已经把 `windows` 依赖改成 `[target.'cfg(windows)'.dependencies]`，
> 所以 Linux 能直接编（如果编译时报 `windows` 相关错误，说明用的是旧版本源码）。

### 2.5 装成 systemd 服务（崩溃自动重启）

仓库里有现成 unit：`scripts/deploy/lrc-relay.service`。

```bash
sudo cp ~/lrc/relay-src/../../../scripts/deploy/lrc-relay.service /etc/systemd/system/ 2>/dev/null ||   sudo tee /etc/systemd/system/lrc-relay.service > /dev/null <<'EOF'
[Unit]
Description=lrc-relay (P2P 信令 + UDP 中继)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ubuntu
WorkingDirectory=/home/ubuntu
ExecStart=/home/ubuntu/lrc/relay-src/target/release/lrc-relay --udp 0.0.0.0:7801 --stats
Restart=always
RestartSec=2
Environment=RUST_BACKTRACE=1
StandardOutput=append:/home/ubuntu/relay.log
StandardError=append:/home/ubuntu/relay.log

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now lrc-relay
sudo systemctl status lrc-relay --no-pager
```

### 2.6 验证服务在跑

```bash
ss -lunp | grep 7801          # 应看到 0.0.0.0:7801 UNCONN
tail -f ~/relay.log           # 应看到"lrc-relay 监听 0.0.0.0:7801（room_ttl=60s）"
                              # 每 5 秒一行 rooms=… relayed_bytes=…
```

### 2.7 从 Windows 验证 UDP 可达性（关键一步）

```powershell
# 简单打 5 个 UDP 包
$udp = New-Object System.Net.Sockets.UdpClient
$b = [System.Text.Encoding]::ASCII.GetBytes('hello-lrc')
1..5 | ForEach-Object { [void]$udp.Send($b, $b.Length, '<VPS_IP>', 7801); Start-Sleep -Milliseconds 200 }
$udp.Close()
```

同时在 VPS 上 `sudo tcpdump -n -U -i any udp port 7801`：
- **有输出** → UDP 通了，继续 §2.8
- **没输出** → 安全组/防火墙没放行，回到 §2.0

### 2.8 跑真正的 P2P 测试（Windows 端）

最省事：用仓库里的脚本，把 relay 指向新 VPS（脚本会在本地起 host+client，但 relay 用远端）：

```powershell
cd D:\lrc
powershell -NoProfile -File scripts\verify-p2p.ps1 -RelayHost <VPS_IP> -HostDisplay 1 -Seconds 10
```

看结果里的 `path=`：
- `direct_udp` → 打洞成功（云上只跑信令，**不花中转带宽**）
- `relay_udp` → 打洞失败走了中继（**属正常**，家庭 CGNAT/对称 NAT 很常见）

手动跑（更贴近真实用法）：

```powershell
# 被控端
target\release\lrc-host.exe --password 1234 --display 1 --stats ^
  --signal <VPS_IP>:7801 --room 1 --p2p auto
# 主控端（另一台机器上把 --connect 换成被控端的内网/公网地址，或干脆省略 --connect 只走 P2P）
target\release\lrc-client.exe --password 1234 --stats ^
  --signal <VPS_IP>:7801 --room 1 --p2p auto
```

> `--p2p auto`：先打洞 → 失败走 relay → 再失败回退 TCP。
> 想强制走 relay 验证中转：两边都加 `--p2p relay`；想只测直连：`--p2p direct`。

---

## 三、本次 VPS 尝试的真实记录（供复盘）

| 步骤 | 结果 |
|---|---|
| SSH 登录 | ✅ `ubuntu` 用户可登录（`root` 被拒）；ping 38–40 ms；Ubuntu 24.04.4 LTS / 2 vCPU / 3.7 GB / 69 GB |
| 安装 Rust | ✅ 1.99.1（中科大镜像）；**直连 crates.io 卡死 20 分钟，换 rsproxy 后正常** |
| 上传 + 编译 | ✅ `cargo build --release -p lrc-relay` 29.17 s，产物 9,384,544 字节 |
| 启动 relay | ✅ 监听 `0.0.0.0:7801`，日志正常，`ss -lunp` 可见 |
| **Windows → VPS 信令（重装前）** | ❌ `REGISTER` 超时 10 s，host 回退 TCP |
| **重装后完整部署** | ✅ Rust 1.99.1、源码上传、`cargo build --release -p lrc-relay` **28.35 s**、systemd 服务 `active`、`ss -lunp` 见 `0.0.0.0:7801` |
| **UDP 入站可达性** | ❌ **确证不通**：tcpdump 抓包 + 多端口 Python 监听，从 Windows 打 UDP 到 7801/3478/45000 → **全部 0 个到达**；同时 **TCP 7800 连接成功**；VPS 内 `ufw inactive`、`iptables INPUT ACCEPT` |
| 结论 | **只差安全组放行 UDP 入站**。代码/服务侧已完全就绪，加完规则即可做真实打洞测试 |

### 重装后的最终状态（2026-11-07 20:40 更新：UDP 已放行，公网链路跑通）

**✅ 全链路已在公网跑通**：Windows 端 host + client 通过 `118.24.79.47:7801` 建链，
`path=relay_udp`、30 fps、1.6–2.3 Mbps、`udp_rtt≈87ms`、12 秒 relay 转发 32.7 MB。

**期间定位到的两个坑（都写进排障表）**：

1. 第一次"打不开"是**安全组规则选成了 TCP**：实测 TCP 7801 返回 RST（放行）、TCP 7800 超时（未放行）、UDP 7801 tcpdump 零包
   → 改成 **协议=UDP** 后立刻通。
2. 中间有一次 **TCP 7800 显示"成功"是假阳性**：当时本机 Clash Verge 的 TUN 在代理，连接是从代理节点发出的；
   退出 Clash 后同一测试立刻超时。**做网络连通性验证时先把 Clash/TUN 关掉**。

3. 打洞（`direct_udp`）在"同一台机器/同一个 NAT"下必然失败（路由器不做 hairpin），
   要验证真实打洞需要**两台不同网络的机器**。

### 服务端当前部署细节

```
# 二进制在 /home/ubuntu/lrc/relay-src/target/release/lrc-relay
systemctl is-active lrc-relay  → active（Restart=always，开机自启）
ss -lunp | grep 7801           → UNCONN 0.0.0.0:7801 (lrc-relay, pid=14424)
二进制                         → /home/ubuntu/lrc/relay-src/target/release/lrc-relay (9,384,544 B)
日志                           → /home/ubuntu/relay.log（"lrc-relay 监听 0.0.0.0:7801（room_ttl=60s）"）
```

**服务端已经全部就绪，唯一缺的是安全组放行 UDP 7801。**

### 结论

**"能不能跑通"完全取决于安全组是否放行 UDP 入站，这一点已确证是当前唯一的阻塞点。**
放行后再按 §2.7 用 tcpdump 复核一次，然后跑 §2.8 的打洞测试。

---

## 四、排障清单（这次踩过的都在这）

| 现象 | 原因 | 处理 |
|---|---|---|
| `REGISTER ... 超时` | 安全组/防火墙没放行 UDP 7801 | 云控制台加入站 UDP 7801；`ufw allow 7801/udp` |
| `cargo build` 卡在 "Updating / Downloading" 二十分钟 | 直连 crates.io 被墙/限速 | 用 rsproxy 镜像（§2.2） |
| `UNPROTECTED PRIVATE KEY FILE` | Windows 上私钥权限过宽 | `icacls` 收紧（§2.1） |
| 构建莫名其妙被 `Terminated`（exit 143） | 我自己跑的 `pkill -f "cargo build"` 把正在跑的构建杀了 | 清理进程时别用太宽的 `pkill -f` 模式 |
| SSH 执行带 `&` 的后台命令时**没有任何输出** | 通道在后台进程启动前就关了 | 把命令写成脚本再 `sh /tmp/x.sh`（`scripts/deploy/` 里都是这么做的） |
| `lrc-client` 报 `--signal 地址非法（:7801）` | 脚本里 relay 地址变量为空 | 确认传了 `-RelayHost` |
| relay 进程莫名消失 | 之前可能收到畸形包 panic（未证实）；也可能是环境被重置 | 用 systemd `Restart=always`（§2.5），并看 `RUST_BACKTRACE` 日志 |

---

## 五、后续待办

1. **跨公网 P2P 打洞成功率**：新 VPS 部署完成后，用两台不同网络的真机测（现在只有一台机器，看到的是同 NAT 回环，打洞结果不代表真实网络）。
2. **relay 的 TCP 兜底**（UDP 被封的企业网）：目前 UDP 全不通就退回 TCP 直连；若被控端在 NAT 后没有公网 IP，TCP 直连也连不上 —— 这时需要"relay over TCP"，还没做。
3. **relay 多客户端/多房间压力**：当前单房间单 host 单 client，自用够，但 `--max-rooms` 只是上限保护，没做压测。
4. **安全**：relay 转发的内容对 VPS 明文可见（无 E2EE）；host 默认监听 `0.0.0.0:7800` 且只有 HMAC 共享口令 —— 跨公网请套 SSH/WireGuard 隧道，或自己加 TLS。
5. 其他已知限制见 [docs/USAGE.md](USAGE.md) §8。
