<#
.SYNOPSIS
  M1 验收 v2：真实 host(DXGI) + 真实 client，PrintWindow 抓客户区像素（不受遮挡影响），
  并分离"首帧"与"静止期"，量静止期每秒钟线上字节数。

.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\verify-e2e-v2.ps1 -HostDisplay 1 -Seconds 10
#>
param(
    [int]$HostDisplay = 1,
    [int]$ObserveSeconds = 10,
    [string]$Password = "1234",
    [int]$Port = 7800,
    [switch]$SkipBuild,
    [switch]$KeepRunning
)

$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$capDir = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $capDir | Out-Null

if (-not $SkipBuild) {
    Push-Location $root
    & cargo build --release -p lrc-host -p lrc-client 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host $_ }
    Pop-Location
}

Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public class W32b {
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
'@

$hostExe = Join-Path $root 'target\release\lrc-host.exe'
$clientExe = Join-Path $root 'target\release\lrc-client.exe'
$hostOut = Join-Path $capDir 'host2.out'
$clientOut = Join-Path $capDir 'client2.out'
foreach ($f in @($hostOut, $clientOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

Write-Host "[1/5] host (display $HostDisplay) ..." -ForegroundColor Cyan
$hostArgs = @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats')
$hostProc = Start-Process -FilePath $hostExe -PassThru -WindowStyle Hidden -ArgumentList $hostArgs -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $capDir 'host2.err')
Start-Sleep -Milliseconds 1000

Write-Host "[2/5] client ..." -ForegroundColor Cyan
$clientArgs = @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats', '--title', 'lrc-viewer-e2e')
$clientProc = Start-Process -FilePath $clientExe -PassThru -ArgumentList $clientArgs -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $capDir 'client2.err')
Start-Sleep -Milliseconds 2500

$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 40; $i++) {
    $clientProc.Refresh()
    if ($clientProc.HasExited) { throw "client 进程已退出，退出码 $($clientProc.ExitCode)；见 captures\client2.err" }
    $h = $clientProc.MainWindowHandle
    if ($null -ne $h -and $h -ne [IntPtr]::Zero) { $hwnd = $h; break }
    Start-Sleep -Milliseconds 200
}
if ($hwnd -eq [IntPtr]::Zero) { throw "client 10 秒内没有主窗口" }
$sb = New-Object System.Text.StringBuilder 256
[void][W32b]::GetWindowTextW($hwnd, $sb, 256)
Write-Host "    hwnd=$hwnd title='$($sb.ToString())' visible=$([W32b]::IsWindowVisible($hwnd))"

# 放到 DISPLAY1（host 采集的是 display1，这样 client 在另一块屏，避免自拍自）
$screens = [System.Windows.Forms.Screen]::AllScreens
$target = $screens | Where-Object { $_.DeviceName -like '*DISPLAY1' } | Select-Object -First 1
if (-not $target) { $target = $screens[0] }
$tb = $target.Bounds
[void][W32b]::SetWindowPos($hwnd, [IntPtr]::Zero, $tb.X + 60, $tb.Y + 60, 1280, 760, 0x0040)
[void][W32b]::SetForegroundWindow($hwnd)
Start-Sleep -Milliseconds 500
$r = New-Object W32b+RECT
[void][W32b]::GetWindowRect($hwnd, [ref]$r)
$cr = New-Object W32b+RECT
[void][W32b]::GetClientRect($hwnd, [ref]$cr)
Write-Host "    window rect = $($r.Left),$($r.Top) - $($r.Right),$($r.Bottom)   client = $($cr.Right)x$($cr.Bottom)"

Write-Host "[3/5] 观察 $ObserveSeconds 秒（前 2 秒算首帧/追帧，之后算静止期）..." -ForegroundColor Cyan
Start-Sleep -Seconds $ObserveSeconds

Write-Host "[4/5] PrintWindow 抓客户区 ..." -ForegroundColor Cyan
$w = $r.Right - $r.Left
$h = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap($w, $h)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [W32b]::PrintWindow($hwnd, $hdc, 2)   # PW_RENDERFULLCONTENT
$g.ReleaseHdc($hdc)
$g.Dispose()
$bmp.Save((Join-Path $capDir 'client-printwindow.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Host "    PrintWindow ok=$ok -> captures\client-printwindow.png"

# 源屏截图（GDI，整块 DISPLAY1）
$srcBmp = New-Object System.Drawing.Bitmap($tb.Width, $tb.Height)
$sg = [System.Drawing.Graphics]::FromImage($srcBmp)
$sg.CopyFromScreen($tb.X, $tb.Y, 0, 0, $srcBmp.Size)
$sg.Dispose()
$srcBmp.Save((Join-Path $capDir 'source-display.png'), [System.Drawing.Imaging.ImageFormat]::Png)
$srcBmp.Dispose()

Write-Host "[5/5] 收尾 ..." -ForegroundColor Cyan
if (-not $KeepRunning) {
    Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
    Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
}
Start-Sleep -Milliseconds 400

Write-Host ""
Write-Host "===== host stats =====" -ForegroundColor Green
if (Test-Path $hostOut) { Get-Content $hostOut }
Write-Host ""
Write-Host "===== client stats =====" -ForegroundColor Green
if (Test-Path $clientOut) { Get-Content $clientOut }

if (Test-Path $hostOut) {
    $lines = @(Get-Content $hostOut | Where-Object { $_ -match 'sec_bytes=' })
    if ($lines.Count -ge 4) {
        $idle = $lines | Select-Object -Skip 2
        $vals = @()
        foreach ($l in $idle) { if ($l -match 'sec_bytes=(\d+)') { $vals += [int]$Matches[1] } }
        if ($vals.Count -gt 0) {
            $min = ($vals | Measure-Object -Minimum).Minimum
            $avg = [int](($vals | Measure-Object -Average).Average)
            Write-Host ""
            Write-Host "静止期每秒线上字节: 采样 $($vals.Count) 秒, min=$min, avg=$avg" -ForegroundColor Green
            if ($min -lt 1024) { Write-Host "✅ 静止零发送成立（存在 <1KB/s 的采样）" -ForegroundColor Green }
            else { Write-Host "⚠️ 静止期仍有 $min B/s 以上流量，需要看 blocks= 判断是内容变化还是 churn" -ForegroundColor Yellow }
        }
    }
}
