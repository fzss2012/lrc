<#
.SYNOPSIS
  办公场景带宽验收：用确定性测试画面（静态 / 打字）驱动被控显示，量 host 每秒线上字节。
  阶段 A = 完全静止（应当 ≈0 B/s，证明"零变化零发送"）
  阶段 B = 连续打字（办公场景，目标 ≤ 1.0 Mbps）

.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\verify-office.ps1 -HostDisplay 1
#>
param(
    [int]$HostDisplay = 1,
    [int]$StaticSeconds = 12,
    [int]$TypingSeconds = 12,
    [string]$Password = "1234",
    [int]$Port = 7800
)

$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$capDir = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $capDir | Out-Null
foreach ($f in @('office-host.out','office-client.out','pat.err')) {
    $p = Join-Path $capDir $f
    if (Test-Path $p) { Remove-Item $p -Force }
}

$hostOut = Join-Path $capDir 'office-host.out'
$clientOut = Join-Path $capDir 'office-client.out'

Write-Host "[1/5] host(display $HostDisplay) + client ..." -ForegroundColor Cyan
$hostProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats', '--fps', '60') -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $capDir 'office-host.err')
Start-Sleep -Milliseconds 800
$clientProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats', '--title', 'lrc-viewer-office') -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $capDir 'office-client.err')
Start-Sleep -Milliseconds 1500

Write-Host "[2/5] 阶段 A：静态画面 $StaticSeconds 秒 ..." -ForegroundColor Cyan
$patA = Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $root 'scripts\testpattern3.ps1'), '-Display', "$HostDisplay", '-Mode', 'static', '-Seconds', "$StaticSeconds")
Start-Sleep -Seconds ($StaticSeconds + 1)
if (-not $patA.HasExited) { Stop-Process -Id $patA.Id -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 800

Write-Host "[3/5] 阶段 B：连续打字 $TypingSeconds 秒 ..." -ForegroundColor Cyan
$patB = Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $root 'scripts\testpattern3.ps1'), '-Display', "$HostDisplay", '-Mode', 'typing', '-Seconds', "$TypingSeconds")
Start-Sleep -Seconds ($TypingSeconds + 1)
if (-not $patB.HasExited) { Stop-Process -Id $patB.Id -Force -ErrorAction SilentlyContinue }

Write-Host "[4/5] 收尾 ..." -ForegroundColor Cyan
Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Write-Host ""
Write-Host "===== host 每秒统计 =====" -ForegroundColor Green
$lines = @()
if (Test-Path $hostOut) {
    $lines = @(Get-Content $hostOut | Where-Object { $_ -match 'sec_bytes=' })
    $lines | ForEach-Object { Write-Host $_ }
}
Write-Host ""
Write-Host "===== client 每秒统计 =====" -ForegroundColor Green
if (Test-Path $clientOut) { Get-Content $clientOut | Select-Object -Last 25 | ForEach-Object { Write-Host $_ } }

Write-Host ""
Write-Host "===== 判定 =====" -ForegroundColor Green
$static = @($lines | Where-Object { $_ -match '^t=(\d+)' -and [int]$Matches[1] -ge 3 -and [int]$Matches[1] -le ($StaticSeconds - 1) })
$typing = @($lines | Where-Object { $_ -match '^t=(\d+)' -and [int]$Matches[1] -gt ($StaticSeconds + 2) -and [int]$Matches[1] -le ($StaticSeconds + $TypingSeconds) })
if ($static.Count -gt 0) {
    $sv = @(); foreach ($l in $static) { if ($l -match 'sec_bytes=(\d+)') { $sv += [int]$Matches[1] } }
    $smin = ($sv | Measure-Object -Minimum).Minimum
    $savg = [int](($sv | Measure-Object -Average).Average)
    Write-Host ("阶段A 静止: 采样{0}秒 min={1}B avg={2}B ({3:F1} kbps)" -f $sv.Count, $smin, $savg, ($savg * 8 / 1000))
    if ($smin -lt 8192) { Write-Host "  ✅ 零发送成立（存在 <8KB/s 的秒）" -ForegroundColor Green } else { Write-Host "  ❌ 静止仍有流量" -ForegroundColor Red }
}
if ($typing.Count -gt 0) {
    $tv = @(); foreach ($l in $typing) { if ($l -match 'sec_bytes=(\d+)') { $tv += [int]$Matches[1] } }
    $tavg = [int](($tv | Measure-Object -Average).Average)
    $tkbps = $tavg * 8 / 1000
    Write-Host ("阶段B 打字: 采样{0}秒 avg={1}B/s = {2:F1} kbps" -f $tv.Count, $tavg, $tkbps)
    if ($tkbps -le 1000) { Write-Host "  ✅ 办公 ≤ 1.0 Mbps" -ForegroundColor Green } else { Write-Host "  ⚠️ 超 1.0 Mbps" -ForegroundColor Yellow }
}
