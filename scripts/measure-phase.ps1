<#
.SYNOPSIS
  单个场景的带宽测量：先起 host+client，再起测试画面，预热后只统计稳定窗口。
.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\measure-phase.ps1 -Mode typing -HostDisplay 1
#>
param(
    [ValidateSet('static', 'typing', 'scroll')]
    [string]$Mode = 'typing',
    [int]$HostDisplay = 1,
    [int]$Warmup = 5,
    [int]$Measure = 10,
    [string]$Password = "1234",
    [int]$Port = 7800
)
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$capDir = Join-Path $root 'captures'
$hostOut = Join-Path $capDir "m-$Mode-host.out"
foreach ($f in @($hostOut, (Join-Path $capDir "m-$Mode-host.err"))) { if (Test-Path $f) { Remove-Item $f -Force } }

$hostProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--listen', "127.0.0.1:$Port", '--password', $Password, '--display', "$HostDisplay", '--stats', '--fps', '30') -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $capDir "m-$Mode-host.err")
Start-Sleep -Milliseconds 700
$clientArgs = @('--connect', "127.0.0.1:$Port", '--password', $Password, '--stats', '--title', "lrc-$Mode")
$clientProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList $clientArgs -RedirectStandardOutput (Join-Path $capDir "m-$Mode-client.out") -RedirectStandardError (Join-Path $capDir "m-$Mode-client.err")
Start-Sleep -Milliseconds 1200

$total = $Warmup + $Measure + 4
$pat = Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $root 'scripts\testpattern3.ps1'), '-Display', "$HostDisplay", '-Mode', $Mode, '-Seconds', "$total")
Start-Sleep -Seconds $total
if (-not $pat.HasExited) { Stop-Process -Id $pat.Id -Force -ErrorAction SilentlyContinue }
Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $hostProc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Write-Host "===== [$Mode] host stats =====" -ForegroundColor Green
$lines = @(Get-Content $hostOut | Where-Object { $_ -match 'sec_bytes=' })
$lines | ForEach-Object { Write-Host $_ }
# 预热之后、测试画面结束之前的窗口
$win = @()
foreach ($l in $lines) {
    if ($l -match '^t=([0-9.]+)') {
        $t = [double]$Matches[1]
        if ($t -gt $Warmup -and $t -le ($Warmup + $Measure)) { $win += $l }
    }
}
if ($win.Count -gt 0) {
    $vals = @()
    foreach ($l in $win) { if ($l -match 'sec_bytes=(\d+)') { $vals += [int]$Matches[1] } }
    $avg = [int](($vals | Measure-Object -Average).Average)
    $min = ($vals | Measure-Object -Minimum).Minimum
    $max = ($vals | Measure-Object -Maximum).Maximum
    Write-Host ""
    Write-Host ("[$Mode] 稳定窗口 {0} 秒: avg={1} B/s ({2:F1} kbps) min={3} max={4}" -f $vals.Count, $avg, ($avg*8/1000), $min, $max) -ForegroundColor Green
}
Write-Host ""
Write-Host "===== [$Mode] client stats =====" -ForegroundColor Green
Get-Content (Join-Path $capDir "m-$Mode-client.out") -ErrorAction SilentlyContinue | Select-Object -Last 14 | ForEach-Object { Write-Host $_ }
