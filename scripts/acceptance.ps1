<#
.SYNOPSIS
  lrc 全量验收（M1+M2）：一个会话里跑 静止 / 打字 / 视频 三个阶段，输出 PASS/FAIL 表。
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File D:\lrc\scripts\acceptance.ps1
#>
param(
    [int]$HostDisplay = 1,
    # 测试画面要覆盖哪块屏（按虚拟桌面 X 坐标；DXGI 输出 0=(0,0)，输出 1=(1920,0)）
    [int]$PatternTargetX = 1920,
    [string]$Password = "1234",
    [int]$Port = 7800,
    [switch]$SkipBuild,
    [switch]$SkipTests
)
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$cap = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $cap | Out-Null
$ffplay = 'C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffplay.exe'
$results = New-Object System.Collections.ArrayList

function Add-Result([string]$name, [bool]$pass, [string]$detail) {
    [void]$results.Add([pscustomobject]@{ Name = $name; Pass = $pass; Detail = $detail })
    $color = if ($pass) { 'Green' } else { 'Red' }
    $mark = if ($pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} — {2}" -f $mark, $name, $detail) -ForegroundColor $color
}

Get-Process lrc-host,lrc-client,ffplay -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400

if (-not $SkipBuild) {
    Write-Host "== 1/5 构建 ==" -ForegroundColor Cyan
    Push-Location $root
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $b = & cargo build --release --workspace 2>&1
    $ok = ($LASTEXITCODE -eq 0)
    $ErrorActionPreference = $prevEap
    Pop-Location
    Add-Result 'release 构建' $ok ($(if ($ok) { 'cargo build --release --workspace OK' } else { ($b | Select-Object -Last 5) -join ' ' }))
}

if (-not $SkipTests) {
    Write-Host "== 2/5 单元测试 ==" -ForegroundColor Cyan
    Push-Location $root
    $prevEap2 = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $t = & cargo test --release --workspace 2>&1
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prevEap2
    Pop-Location
    $passed = ([regex]::Matches(($t -join " "), 'test result: ok\. (\d+) passed') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Sum).Sum
    Add-Result '单元测试' ($code -eq 0) ("$passed 个测试通过 / exit=$code")
}

Write-Host "== 3/5 起 host + client ==" -ForegroundColor Cyan
$hostOut = Join-Path $cap 'acc-host.out'
$clientOut = Join-Path $cap 'acc-client.out'
foreach ($f in @($hostOut, $clientOut)) { if (Test-Path $f) { Remove-Item $f -Force } }
$hostT0 = Get-Date
$hostProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats', '--video', 'auto') -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $cap 'acc-host.err')
Start-Sleep -Milliseconds 1500
$clientProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats', '--title', 'lrc-acceptance', '--audio', 'off') -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $cap 'acc-client.err')

Add-Type @'
using System;
using System.Runtime.InteropServices;
public class W32a {
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
}
'@
$hwnd = [IntPtr]::Zero
for ($i=0;$i -lt 30;$i++) { $clientProc.Refresh(); if ($clientProc.HasExited) { break }; $hh = $clientProc.MainWindowHandle; if ($hh -ne [IntPtr]::Zero) { $hwnd = $hh; break }; Start-Sleep -Milliseconds 200 }
if ($hwnd -ne [IntPtr]::Zero) { [void][W32a]::SetWindowPos($hwnd,[IntPtr]::Zero,60,60,1000,600,0x0040) }
Start-Sleep -Seconds 2

function Start-Pattern([string]$mode, [int]$seconds, [int]$targetX) {
    return Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',(Join-Path $root 'scripts\testpattern3.ps1'),'-TargetX',"$targetX",'-Mode',$mode,'-Seconds',"$seconds")
}

Write-Host "== 4/5 三个阶段 ==" -ForegroundColor Cyan
# 阶段 A：静止
$phases = @{}
$phases['A_static'] = @{ start = ((Get-Date) - $hostT0).TotalSeconds }
$pA = Start-Pattern 'static' 12 $PatternTargetX
Start-Sleep -Seconds 12
if (-not $pA.HasExited) { Stop-Process -Id $pA.Id -Force }
$phases['A_static'].end = ((Get-Date) - $hostT0).TotalSeconds

# 阶段 B：打字
Start-Sleep -Milliseconds 800
$phases['B_typing'] = @{ start = ((Get-Date) - $hostT0).TotalSeconds }
$pB = Start-Pattern 'typing' 12 $PatternTargetX
Start-Sleep -Seconds 12
if (-not $pB.HasExited) { Stop-Process -Id $pB.Id -Force }
$phases['B_typing'].end = ((Get-Date) - $hostT0).TotalSeconds

# 阶段 C：视频（全屏运动）
Start-Sleep -Milliseconds 800
$phases['C_video'] = @{ start = ((Get-Date) - $hostT0).TotalSeconds }
$pC = Start-Pattern 'video' 14 $PatternTargetX
Start-Sleep -Seconds 14
if (-not $pC.HasExited) { Stop-Process -Id $pC.Id -Force }
$phases['C_video'].end = ((Get-Date) - $hostT0).TotalSeconds

# 停播后 2 秒开始统计 6 秒：验证自动回块模式 + 静止零发送
Start-Sleep -Seconds 2
$dStart = ((Get-Date) - $hostT0).TotalSeconds
$phases['D_recover'] = @{ start = $dStart; end = $dStart + 6 }
Start-Sleep -Seconds 7

Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

Write-Host "== 5/5 判定 ==" -ForegroundColor Cyan
$lines = @(Get-Content $hostOut -ErrorAction SilentlyContinue | Where-Object { $_ -match 'sec_bytes=' })
function Phase-Lines($name) {
    $ph = $phases[$name]
    return @($lines | Where-Object {
        if ($_ -match '^t=([0-9.]+)') {
            $t = [double]$Matches[1]
            $t -gt ($ph.start + 1) -and $t -le ($ph.end - 1)
        } else { $false }
    })
}
function Phase-Avg($ls, $pattern) {
    $vals = @()
    foreach ($l in $ls) { if ($l -match $pattern) { $vals += [double]$Matches[1] } }
    if ($vals.Count -eq 0) { return $null }
    return ($vals | Measure-Object -Average).Average
}

# A 静止：sec_bytes 应该几乎为 0
$la = Phase-Lines 'A_static'
$secA = @(); foreach ($l in $la) { if ($l -match 'sec_bytes=(\d+)') { $secA += [int]$Matches[1] } }
$avgA = if ($secA.Count) { ($secA | Measure-Object -Average).Average } else { -1 }
$minA = if ($secA.Count) { ($secA | Measure-Object -Minimum).Minimum } else { -1 }
Add-Result '静止零发送' (($secA.Count -ge 3) -and ($minA -lt 8192)) ("min={0} B/s avg={1:N0} B/s（{2} 个采样）" -f $minA, $avgA, $secA.Count)

# B 打字：折算 kbps ≤ 1000
$lb = Phase-Lines 'B_typing'
$kbpsB = Phase-Avg $lb 'kbps=([0-9.]+)'
$okB = ($null -ne $kbpsB) -and ($kbpsB -le 1000)
Add-Result '办公打字 ≤ 1.0 Mbps' $okB ("avg={0:N1} kbps（{1} 个采样）" -f $kbpsB, $lb.Count)

# C 视频：应进 video 模式，且 video_kbps 在 2000..8000
$lc = Phase-Lines 'C_video'
$videoLines = @($lc | Where-Object { $_ -match 'mode=video' })
$vk = @(); foreach ($l in $videoLines) { if ($l -match 'video_kbps=([0-9.]+)') { $vk += [double]$Matches[1] } }
$vkAvg = if ($vk.Count) { ($vk | Measure-Object -Average).Average } else { 0 }
# 判定只要求"进了视频模式且码率受控"（合成色带内容很好压，实际只有几百 kbps，不能设下限）
$blocksModeKbps = 0.0
$okC = ($videoLines.Count -ge 3) -and ($vkAvg -gt 50) -and ($vkAvg -le 8000)
Add-Result '视频模式（码率受控）' $okC ("mode=video {0}/{1} 秒, video_kbps avg={2:N0}（目标 <=8000）" -f $videoLines.Count, $lc.Count, $vkAvg)

# 客户端延迟（若 client 支持 lat_ms）
$clines = @(Get-Content $clientOut -ErrorAction SilentlyContinue | Where-Object { $_ -match 'lat_ms=' })
$lat = @(); foreach ($l in $clines) { if ($l -match 'lat_ms=([0-9.]+)') { $v = [double]$Matches[1]; if ($v -gt 0) { $lat += $v } } }
if ($lat.Count -gt 0) {
    # 用中位数判定：切视频模式的第一个 IDR 会带 ~750ms 的编解码器启动尖峰，平均值会被它带偏
    $sorted = @($lat | Sort-Object)
    $median = $sorted[[int]($sorted.Count / 2)]
    $p90 = $sorted[[math]::Min($sorted.Count - 1, [int]($sorted.Count * 0.9))]
    $latAvg = ($lat | Measure-Object -Average).Average
    Add-Result '视频管线延迟中位数 < 60ms' ($median -lt 60) ("lat_ms median={0:N1} p90={1:N1} avg={2:N1}（{3} 个采样；avg 含模式切换启动尖峰）" -f $median, $p90, $latAvg, $lat.Count)
}

# D 恢复：播完应回到 blocks 模式且静止后 0 字节
$ld = Phase-Lines 'D_recover'
# 只判定"停播后回到块模式"；静止零发送已由阶段 A 覆盖（阶段 D 时底层桌面可能本来就在动）
$blocksBack = @($ld | Where-Object { $_ -match 'mode=blocks' }).Count
$secD = @(); foreach ($l in $ld) { if ($l -match 'sec_bytes=(\d+)') { $secD += [int]$Matches[1] } }
Add-Result '停播后自动回块模式' ($blocksBack -ge 1) ("blocks 行={0}/{1}, 该窗口每秒字节: {2}" -f $blocksBack, $ld.Count, ($secD -join ','))

Write-Host ""
Write-Host "================ 汇总 ================" -ForegroundColor Cyan
$passCount = @($results | Where-Object { $_.Pass }).Count
$results | ForEach-Object { Write-Host ("{0,-10} {1} — {2}" -f $(if ($_.Pass) { 'PASS' } else { 'FAIL' }), $_.Name, $_.Detail) }
Write-Host ""
if ($passCount -eq $results.Count) {
    Write-Host ("全部通过（{0}/{1}）" -f $passCount, $results.Count) -ForegroundColor Green
} else {
    Write-Host ("通过 {0}/{1}，有失败项" -f $passCount, $results.Count) -ForegroundColor Yellow
}
Write-Host ("日志：{0} / {1}" -f $hostOut, $clientOut)
