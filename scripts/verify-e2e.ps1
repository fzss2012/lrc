<#
.SYNOPSIS
  M1 端到端验收：起 host（采集 display N）+ client，量静止码率、抓两侧截图比像素。
.EXAMPLE
  pwsh -File D:\lrc\scripts\verify-e2e.ps1 -HostDisplay 0 -Seconds 12
#>
param(
    [int]$HostDisplay = 0,
    [int]$Seconds = 12,
    [string]$Password = "1234",
    [int]$Port = 7800,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$capDir = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $capDir | Out-Null

$hostExe = Join-Path $root 'target\release\lrc-host.exe'
$clientExe = Join-Path $root 'target\release\lrc-client.exe'

if (-not $SkipBuild) {
    Write-Host "[1/6] cargo build --release ..." -ForegroundColor Cyan
    Push-Location $root
    & cargo build --release 2>&1 | Select-Object -Last 5 | ForEach-Object { Write-Host "    $_" }
    Pop-Location
}
foreach ($exe in @($hostExe, $clientExe)) {
    if (-not (Test-Path $exe)) { throw "缺少 $exe" }
}

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class W32 {
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
'@

$hostOut = Join-Path $capDir 'host.out'
$clientOut = Join-Path $capDir 'client.out'
foreach ($f in @($hostOut, $clientOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

Write-Host "[2/6] 启动 host（display $HostDisplay）..." -ForegroundColor Cyan
$hostArgs = @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats')
$hostProc = Start-Process -FilePath $hostExe -PassThru -WindowStyle Hidden -ArgumentList $hostArgs -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $capDir 'host.err')
Start-Sleep -Milliseconds 1200
if ($hostProc.HasExited) { throw "host 启动即退出，见 captures\host.err" }

Write-Host "[3/6] 启动 client ..." -ForegroundColor Cyan
$clientArgs = @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats')
$clientProc = Start-Process -FilePath $clientExe -PassThru -ArgumentList $clientArgs -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $capDir 'client.err')
Start-Sleep -Milliseconds 2500

$clientProc.Refresh()
$hwnd = $clientProc.MainWindowHandle
if ($hwnd -ne [IntPtr]::Zero) {
    $screens = [System.Windows.Forms.Screen]::AllScreens
    $target = $screens | Where-Object { -not $_.Primary } | Select-Object -First 1
    if (-not $target) { $target = $screens[0] }
    $b = $target.Bounds
    [void][W32]::SetWindowPos($hwnd, [IntPtr]::Zero, $b.X + 40, $b.Y + 40, 1600, 900, 0x0040)
    Write-Host "    client 窗口 -> $($target.DeviceName) 位置 $($b.X + 40),$($b.Y + 40) 1600x900"
} else {
    Write-Host "    [警告] 拿不到 client 窗口句柄" -ForegroundColor Yellow
}

Write-Host "[4/6] 观察 $Seconds 秒 ..." -ForegroundColor Cyan
Start-Sleep -Seconds $Seconds

Write-Host "[5/6] 抓截图 ..." -ForegroundColor Cyan
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
$desk = New-Object System.Drawing.Bitmap($vs.Width, $vs.Height)
$g = [System.Drawing.Graphics]::FromImage($desk)
$g.CopyFromScreen($vs.X, $vs.Y, 0, 0, $desk.Size)
$g.Dispose()
$deskPath = Join-Path $capDir 'desktop.png'
$desk.Save($deskPath, [System.Drawing.Imaging.ImageFormat]::Png)
$desk.Dispose()

$clientRectOk = $false
if ($hwnd -ne [IntPtr]::Zero) {
    $r = New-Object W32+RECT
    if ([W32]::GetWindowRect($hwnd, [ref]$r)) {
        $w = $r.Right - $r.Left
        $h = $r.Bottom - $r.Top
        if ($w -gt 0 -and $h -gt 0) {
            $cb = New-Object System.Drawing.Bitmap($w, $h)
            $cg = [System.Drawing.Graphics]::FromImage($cb)
            $cg.CopyFromScreen($r.Left, $r.Top, 0, 0, $cb.Size)
            $cg.Dispose()
            $cb.Save((Join-Path $capDir 'client-window.png'), [System.Drawing.Imaging.ImageFormat]::Png)
            $cb.Dispose()
            $clientRectOk = $true
            Write-Host "    client 窗口截图 $w x $h"
        }
    }
}

Write-Host "[6/6] 收尾 ..." -ForegroundColor Cyan
Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300

Write-Host ""
Write-Host "===== host stats（后 20 行） =====" -ForegroundColor Green
if (Test-Path $hostOut) { Get-Content $hostOut -Tail 20 }
Write-Host ""
Write-Host "===== client stats（后 20 行） =====" -ForegroundColor Green
if (Test-Path $clientOut) { Get-Content $clientOut -Tail 20 }
Write-Host ""
Write-Host "截图: $deskPath" -ForegroundColor Green
if ($clientRectOk) { Write-Host "      $(Join-Path $capDir 'client-window.png')" -ForegroundColor Green }

if (Test-Path $hostOut) {
    $lines = Get-Content $hostOut | Where-Object { $_ -match 'kbps=' }
    $tail = @($lines | Select-Object -Last 5)
    $idle = $true
    foreach ($l in $tail) {
        if ($l -match 'kbps=\s*([0-9.]+)') {
            if ([double]$Matches[1] -gt 5.0) { $idle = $false }
        }
    }
    Write-Host ""
    if ($idle -and $tail.Count -ge 3) {
        Write-Host "✅ 静止零发送：最后 $($tail.Count) 个采样 kbps <= 5" -ForegroundColor Green
    } else {
        Write-Host "⚠️ 静止期仍有流量（或采样不足）" -ForegroundColor Yellow
    }
}
