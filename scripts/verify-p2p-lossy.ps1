<#
.SYNOPSIS
    M4 弱网验收：在 P2P/relay 链路上注入丢包+延迟，验证会话不中断、码率同量级、断线能重连。

.DESCRIPTION
    拓扑（全部本机 127.0.0.1）：

        lrc-client ---(信令+媒体)---> udp-proxy(丢包/延迟) ---> lrc-relay <--- lrc-host
                                       ^                                         ^
                                       +--- relay 转发回来的媒体包也走这里 -------+

    - relay 跑 127.0.0.1:7801（--stats 每 5s 一行）
    - proxy 跑 127.0.0.1:7901 -> 127.0.0.1:7801（--loss N --delay-ms 20）
    - host 的 --signal 直连 relay；client 的 --signal 指向 proxy
      => relay 看到的 client 地址就是 proxy，于是两个方向的媒体包都会穿过 proxy（双向弱网）
    - 两端都用 --p2p relay（punch_timeout=0 跳过打洞），保证路径确定为 relay_udp

    判定：
      1) 会话全程不中断：client 有 stats 输出、decoded_blocks 增长、reconnects=0
      2) 码率同量级：loss=5/10 的 kbps <= 1.5 x loss=0 的 kbps
      3) 全丢（--loss 100）5 秒后恢复链路，client 必须重连并最终恢复画面

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\verify-p2p-lossy.ps1 -Display 1 -Seconds 10
#>
param(
    [int]$Display = 0,
    [string]$Password = "lrc-test",
    [int]$Room = 7,
    # 用字符串接收再自己拆：powershell -File 传 "0,5,10" 时 [double[]] 会按区域设置解析成别的数
    [string]$LossList = "0,5,10",
    # 弱网下"首屏整帧"要多轮重传才到齐（实测 5% 丢包要 10s+），
    # 所以默认跑 15s 并丢掉前 WarmupSeconds 秒的预热窗口
    [int]$Seconds = 15,
    [int]$WarmupSeconds = 8,
    [int]$DelayMs = 20,
    [string]$OutDir = "D:\lrc\target\p2p-lossy",
    [ValidateSet('static', 'typing', 'scroll', 'video')]
    [string]$PatternMode = 'scroll',
    [int]$PatternTargetX = -1,
    # lrc-udp 的 DEAD_AFTER 是 10s（连续这么久没收到对端任何包才判死），
    # 所以全丢窗口必须 > 10s 才能触发断线判定
    [int]$DeadLossSeconds = 14,
    [switch]$SkipDisconnectTest
)

$ErrorActionPreference = "Stop"
$root = "D:\lrc"
$bin = Join-Path $root "target\release"
$relayAddr = "127.0.0.1:7801"
$proxyAddr = "127.0.0.1:7901"
$tcpAddr = "127.0.0.1:7800"

function Write-Step($msg) { Write-Host ""; Write-Host ("=== " + $msg + " ===") -ForegroundColor Cyan }

function Start-Logged($exe, $argsArr, $tag, $dir) {
    $out = Join-Path $dir ("{0}.out.log" -f $tag)
    $err = Join-Path $dir ("{0}.err.log" -f $tag)
    Remove-Item $out, $err -ErrorAction SilentlyContinue
    $p = Start-Process -FilePath $exe -ArgumentList $argsArr -PassThru -NoNewWindow -RedirectStandardOutput $out -RedirectStandardError $err
    return @{ Proc = $p; Out = $out; Err = $err; Tag = $tag }
}

function Stop-Logged($h) {
    if ($null -eq $h) { return }
    try {
        if (-not $h.Proc.HasExited) { Stop-Process -Id $h.Proc.Id -Force -ErrorAction SilentlyContinue }
    } catch { }
    try { $h.Proc.WaitForExit(1500) | Out-Null } catch { }
}

function Get-LastLine($file, $pattern) {
    if (-not (Test-Path $file)) { return "" }
    $m = Select-String -Path $file -Pattern $pattern -ErrorAction SilentlyContinue | Select-Object -Last 1
    if ($null -eq $m) { return "" }
    return $m.Line
}

function Get-Field($line, $name) {
    if ([string]::IsNullOrWhiteSpace($line)) { return "" }
    $m = [regex]::Match($line, ([regex]::Escape($name) + "=([^\s]+)"))
    if ($m.Success) { return $m.Groups[1].Value }
    return ""
}

function As-Double($v) { $d = 0.0; if ([double]::TryParse([string]$v, [ref]$d)) { return $d }; return 0.0 }
function As-Long($v) { $l = 0L; if ([int64]::TryParse([string]$v, [ref]$l)) { return $l }; return 0L }

foreach ($exe in @("lrc-relay.exe", "lrc-host.exe", "lrc-client.exe", "udp-proxy.exe")) {
    $p = Join-Path $bin $exe
    if (-not (Test-Path $p)) { Write-Host ("缺少 " + $p + "，先 cargo build --release --workspace") -ForegroundColor Red; exit 2 }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

$losses = @($LossList.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" } | ForEach-Object { [double]$_ })
if ($losses.Count -eq 0) { Write-Host "LossList 为空" -ForegroundColor Red; exit 2 }
Write-Host ("将测试丢包率: " + ($losses -join ", ") + " %")

$rows = @()

foreach ($loss in $losses) {
    Write-Step ("loss=" + $loss + "%  delay=" + $DelayMs + "ms  " + $Seconds + "s")
    $dir = Join-Path $OutDir ("loss" + [int]$loss)
    New-Item -ItemType Directory -Force -Path $dir | Out-Null

    $relay = $null; $proxy = $null; $hostp = $null; $client = $null; $pat = $null
    try {
        $relay = Start-Logged (Join-Path $bin "lrc-relay.exe") @("--udp", $relayAddr, "--stats", "--room-ttl", "120") "relay" $dir
        Start-Sleep -Milliseconds 400
        $proxy = Start-Logged (Join-Path $bin "udp-proxy.exe") @("--listen", $proxyAddr, "--target", $relayAddr, "--loss", "$loss", "--delay-ms", "$DelayMs", "--quiet") "proxy" $dir
        Start-Sleep -Milliseconds 300

        $hostp = Start-Logged (Join-Path $bin "lrc-host.exe") @("--password", $Password, "--signal", $relayAddr, "--room", "$Room", "--p2p", "relay", "--display", "$Display", "--stats") "host" $dir
        Start-Sleep -Milliseconds 1200

        # 测试画面：桌面静止时根本没有媒体流量，必须造真实内容变化
        $patArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $root "scripts\testpattern3.ps1"), "-Display", "$Display", "-TargetX", "$PatternTargetX", "-Mode", $PatternMode, "-Seconds", "$($Seconds + 8)")
        $pat = Start-Process -FilePath "powershell.exe" -ArgumentList $patArgs -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $dir "pattern.out.log") -RedirectStandardError (Join-Path $dir "pattern.err.log")
        Start-Sleep -Seconds 2

        $client = Start-Logged (Join-Path $bin "lrc-client.exe") @("--connect", $tcpAddr, "--password", $Password, "--signal", $proxyAddr, "--room", "$Room", "--p2p", "relay", "--stats") "client" $dir
        Start-Sleep -Seconds $Seconds

        $clientLines = @(Select-String -Path $client.Out -Pattern "^fps=" -ErrorAction SilentlyContinue)
        # 只统计预热之后的稳定窗口（首屏整帧在丢包下要多轮重传，属于启动过程）
        $warm = [math]::Min($WarmupSeconds, [math]::Max(0, $clientLines.Count - 2))
        $first = if ($clientLines.Count -gt 0) { $clientLines[$warm].Line } else { "" }
        $last = if ($clientLines.Count -gt 0) { $clientLines[-1].Line } else { "" }
        $hostLast = Get-LastLine $hostp.Out "^t="
        $relayLast = Get-LastLine $relay.Out "relayed_bytes="

        $fps = As-Double (Get-Field $last "fps")
        $blkFirst = As-Long (Get-Field $first "decoded_blocks")
        $blkLast = As-Long (Get-Field $last "decoded_blocks")
        $kbps = As-Double (Get-Field $last "recv_kbps")
        $reconn = As-Long (Get-Field $last "reconnects")
        $cPath = Get-Field $last "path"
        $cRtt = Get-Field $last "udp_rtt"
        $cLoss = Get-Field $last "udp_loss"
        $hKbps = As-Double (Get-Field $hostLast "kbps")
        $hRtt = Get-Field $hostLast "udp_rtt"
        $hLoss = Get-Field $hostLast "udp_loss"
        $hPath = Get-Field $hostLast "path"
        $relayed = As-Long (Get-Field $relayLast "relayed_bytes")

        $blocksGrew = ($blkLast -gt $blkFirst) -and ($blkLast -gt 0)
        # 会话活着 = 有持续统计输出 + 没有触发重连 + 画面在动（fps>0 或解码块在增长）
        $alive = ($clientLines.Count -gt 3) -and ($reconn -eq 0) -and (($fps -gt 0) -or $blocksGrew)

        $rows += [pscustomobject]@{
            loss = $loss; path = $(if ($cPath) { $cPath } else { $hPath }); client_fps = $fps;
            blocks = $blkLast; grew = $blocksGrew; c_kbps = $kbps; h_kbps = $hKbps;
            c_rtt = $cRtt; h_rtt = $hRtt; c_loss = $cLoss; h_loss = $hLoss;
            reconnects = $reconn; relayed = $relayed; alive = $alive
        }
        Write-Host ("  path=" + $cPath + " fps=" + $fps + " blocks=" + $blkFirst + "->" + $blkLast + " kbps(client)=" + [math]::Round($kbps, 1) + " kbps(host)=" + [math]::Round($hKbps, 1) + " udp_rtt=" + $cRtt + " udp_loss=" + $cLoss + " reconnects=" + $reconn + " relayed_bytes=" + $relayed)
    }
    finally {
        Stop-Logged $client; Stop-Logged $hostp; Stop-Logged $proxy; Stop-Logged $relay
        if ($null -ne $pat) { try { Stop-Process -Id $pat.Id -Force -ErrorAction SilentlyContinue } catch { } }
        Get-Process powershell -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowTitle -like "*testpattern*" } | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 400
    }
}

$disc = $null
if (-not $SkipDisconnectTest) {
    Write-Step ("断线重连：--loss 100 跑 " + $DeadLossSeconds + " 秒 -> 换回 --loss 0 -> 期待客户端重连并恢复画面")
    $dir = Join-Path $OutDir "disconnect"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $relay = $null; $proxy = $null; $proxy2 = $null; $hostp = $null; $client = $null; $pat = $null
    try {
        $relay = Start-Logged (Join-Path $bin "lrc-relay.exe") @("--udp", $relayAddr, "--stats", "--room-ttl", "120") "relay" $dir
        Start-Sleep -Milliseconds 400
        $proxy = Start-Logged (Join-Path $bin "udp-proxy.exe") @("--listen", $proxyAddr, "--target", $relayAddr, "--loss", "0", "--delay-ms", "0", "--quiet") "proxy0" $dir
        Start-Sleep -Milliseconds 300
        $hostp = Start-Logged (Join-Path $bin "lrc-host.exe") @("--password", $Password, "--signal", $relayAddr, "--room", "$Room", "--p2p", "relay", "--display", "$Display", "--stats") "host" $dir
        Start-Sleep -Milliseconds 1200
        $patArgs = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", (Join-Path $root "scripts\testpattern3.ps1"), "-Display", "$Display", "-TargetX", "$PatternTargetX", "-Mode", $PatternMode, "-Seconds", "40")
        $pat = Start-Process -FilePath "powershell.exe" -ArgumentList $patArgs -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $dir "pattern.out.log") -RedirectStandardError (Join-Path $dir "pattern.err.log")
        Start-Sleep -Seconds 2
        $client = Start-Logged (Join-Path $bin "lrc-client.exe") @("--connect", $tcpAddr, "--password", $Password, "--signal", $proxyAddr, "--room", "$Room", "--p2p", "relay", "--stats") "client" $dir
        Start-Sleep -Seconds 4

        $before = Get-LastLine $client.Out "^fps="
        Write-Host ("  断线前: " + $before)

        Stop-Logged $proxy
        $proxy = $null
        Start-Sleep -Milliseconds 200
        $proxy2 = Start-Logged (Join-Path $bin "udp-proxy.exe") @("--listen", $proxyAddr, "--target", $relayAddr, "--loss", "100", "--delay-ms", "0", "--quiet") "proxy100" $dir
        Start-Sleep -Seconds $DeadLossSeconds
        $during = Get-LastLine $client.Out "^fps="
        $sawDisc = (Select-String -Path $client.Err -Pattern "连接断开" -ErrorAction SilentlyContinue | Select-Object -First 1)
        Write-Host ("  全丢 " + $DeadLossSeconds + "s 后: " + $during)
        if ($null -ne $sawDisc) { Write-Host ("  客户端已判死并重连: " + $sawDisc.Line.Trim()) -ForegroundColor Yellow }
        else { Write-Host "  客户端日志里还没出现『连接断开』" -ForegroundColor Yellow }

        Stop-Logged $proxy2
        $proxy2 = $null
        Start-Sleep -Milliseconds 200
        $proxy = Start-Logged (Join-Path $bin "udp-proxy.exe") @("--listen", $proxyAddr, "--target", $relayAddr, "--loss", "0", "--delay-ms", "0", "--quiet") "proxy0b" $dir
        # 重连退避是 1/2/4/8/10s，且需要等 host 也判死并重新登记，给足 30s
        Start-Sleep -Seconds 30

        $after = Get-LastLine $client.Out "^fps="
        $reconnAfter = As-Long (Get-Field $after "reconnects")
        $fpsAfter = As-Double (Get-Field $after "fps")
        $blkAfter = As-Long (Get-Field $after "decoded_blocks")
        $blkDuring = As-Long (Get-Field $during "decoded_blocks")
        $recovered = ($reconnAfter -ge 1) -and (($fpsAfter -gt 0) -or ($blkAfter -gt $blkDuring))
        Write-Host ("  恢复后: " + $after)
        if ($recovered) { Write-Host ("  -> reconnects=" + $reconnAfter + " fps=" + $fpsAfter + " 恢复=是") -ForegroundColor Green }
        else { Write-Host ("  -> reconnects=" + $reconnAfter + " fps=" + $fpsAfter + " 恢复=否") -ForegroundColor Red }
        $disc = [pscustomobject]@{ before_fps = (As-Double (Get-Field $before "fps")); during_fps = (As-Double (Get-Field $during "fps")); after_fps = $fpsAfter; reconnects = $reconnAfter; recovered = $recovered }
    }
    finally {
        Stop-Logged $client; Stop-Logged $hostp; Stop-Logged $proxy; Stop-Logged $proxy2; Stop-Logged $relay
        if ($null -ne $pat) { try { Stop-Process -Id $pat.Id -Force -ErrorAction SilentlyContinue } catch { } }
    }
}

Write-Step "M4 弱网验收汇总"
$rows | Format-Table -AutoSize loss, path, client_fps, blocks, grew, c_kbps, h_kbps, c_rtt, c_loss, reconnects, relayed, alive | Out-String | Write-Host

$base = ($rows | Where-Object { $_.loss -eq 0 } | Select-Object -First 1)
$fail = @()
foreach ($r in $rows) {
    if (-not $r.alive) { $fail += ("loss=" + $r.loss + "% 会话中断/重连") }
    if ($null -ne $base -and $base.c_kbps -gt 0 -and $r.loss -gt 0 -and $r.c_kbps -gt $base.c_kbps * 1.5) {
        $fail += ("loss=" + $r.loss + "% 码率 " + [math]::Round($r.c_kbps, 1) + " 超过基线 " + [math]::Round($base.c_kbps, 1) + " 的 1.5 倍")
    }
}
if ($null -ne $disc -and -not $disc.recovered) { $fail += "全丢后未能重连恢复" }

Write-Host ""
if ($fail.Count -eq 0) {
    Write-Host "验收通过：所有档位会话不中断、码率同量级、断线可重连" -ForegroundColor Green
    exit 0
} else {
    foreach ($f in $fail) { Write-Host ("验收失败：" + $f) -ForegroundColor Red }
    exit 1
}
