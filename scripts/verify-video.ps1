<#
.SYNOPSIS
  M2 视频通道验收：块模式 → 播视频（应自动切视频模式、码率落到 3-6 Mbps）→ 停播（应切回块模式）。
.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\verify-video.ps1 -HostDisplay 1 -PlaySeconds 20
#>
param(
    [int]$HostDisplay = 1,
    [int]$Warmup = 4,
    [int]$PlaySeconds = 20,
    [int]$Settle = 8,
    [string]$Password = "1234",
    [int]$Port = 7800,
    [string]$FfplayPath = 'C:\Program Files\ffmpeg-8.0.1-essentials_build\bin\ffplay.exe',
    # 必须是"整屏都在动"的源，否则 changed_ratio 上不去、不会切视频模式（testsrc 大部分是静态彩条）
    [string]$VideoSource = 'mandelbrot=s=1920x1080:r=30'
)
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$cap = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $cap | Out-Null
$hostOut = Join-Path $cap 'video-host.out'
$clientOut = Join-Path $cap 'video-client.out'
foreach ($f in @($hostOut, $clientOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Get-Process lrc-host,lrc-client,ffplay -ErrorAction SilentlyContinue | Stop-Process -Force

Write-Host "[1/5] 起 host(--video auto) + client ..." -ForegroundColor Cyan
$hostProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats', '--video', 'auto') -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $cap 'video-host.err')
Start-Sleep -Milliseconds 1500
$clientProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats', '--title', 'lrc-video-check') -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $cap 'video-client.err')
Start-Sleep -Milliseconds 2500

Add-Type @'
using System;
using System.Runtime.InteropServices;
public class W32v {
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
'@
$hwnd=[IntPtr]::Zero
for ($i=0;$i -lt 30;$i++){ $clientProc.Refresh(); if ($clientProc.HasExited) { throw 'client exited' }; $hh=$clientProc.MainWindowHandle; if ($hh -ne [IntPtr]::Zero){$hwnd=$hh;break}; Start-Sleep -Milliseconds 200 }
if ($hwnd -ne [IntPtr]::Zero) { [void][W32v]::SetWindowPos($hwnd,[IntPtr]::Zero,1980,80,1280,760,0x0040) }

Write-Host "[2/5] 块模式基线 $Warmup 秒 ..." -ForegroundColor Cyan
Start-Sleep -Seconds $Warmup

Write-Host "[3/5] 在 display $HostDisplay 上播测试视频 $PlaySeconds 秒 ..." -ForegroundColor Cyan
$scr = [System.Windows.Forms.Screen]::AllScreens | Where-Object { $_.Bounds.X -eq 1920 } | Select-Object -First 1
$x = if ($scr) { $scr.Bounds.X } else { 0 }
$y = if ($scr) { $scr.Bounds.Y } else { 0 }
$ff = Start-Process -FilePath $FfplayPath -PassThru -ArgumentList @('-hide_banner','-loglevel','error','-f','lavfi','-i',$VideoSource,'-t',"$PlaySeconds",'-noborder','-left',"$x",'-top',"$y",'-x','1920','-y','1080')
Start-Sleep -Seconds $PlaySeconds
if (-not $ff.HasExited) { Stop-Process -Id $ff.Id -Force -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 500

Write-Host "[4/5] 停播后观察 $Settle 秒（应切回块模式）..." -ForegroundColor Cyan
Start-Sleep -Seconds $Settle

Write-Host "[5/5] 收尾 ..." -ForegroundColor Cyan
Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Write-Host ""
Write-Host "===== host stats =====" -ForegroundColor Green
Get-Content $hostOut -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
Write-Host ""
Write-Host "===== client stats（后 25 行） =====" -ForegroundColor Green
Get-Content $clientOut -ErrorAction SilentlyContinue | Select-Object -Last 25 | ForEach-Object { Write-Host $_ }

Write-Host ""
Write-Host "===== 判定 =====" -ForegroundColor Green
$lines = @(Get-Content $hostOut -ErrorAction SilentlyContinue | Where-Object { $_ -match 'mode=' })
$videoLines = @($lines | Where-Object { $_ -match 'mode=video' })
$blockLines = @($lines | Where-Object { $_ -match 'mode=blocks' })
Write-Host ("采样: 总 {0} 行, mode=video {1} 行, mode=blocks {2} 行" -f $lines.Count, $videoLines.Count, $blockLines.Count)
if ($videoLines.Count -gt 0) {
    $vk = @()
    foreach ($l in $videoLines) { if ($l -match 'video_kbps=([0-9.]+)') { $vk += [double]$Matches[1] } }
    if ($vk.Count -gt 0) {
        $avg = ($vk | Measure-Object -Average).Average
        Write-Host ("视频模式平均 video_kbps = {0:F1} （目标 3000-6500）" -f $avg)
        if ($avg -ge 2000 -and $avg -le 8000) { Write-Host "  ✅ 视频模式码率在预期区间" -ForegroundColor Green }
        else { Write-Host "  ⚠️ 视频模式码率不在预期区间" -ForegroundColor Yellow }
    }
} else {
    Write-Host "  ❌ 从未进入视频模式" -ForegroundColor Red
}
