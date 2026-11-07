# 跨公网 relay 下跑视频模式，看码率/帧率/丢包
param([string]$RelayHost = '118.24.79.47', [int]$RelayPort = 7801, [int]$Room = 21,
      [string]$Password = '1234', [int]$Seconds = 15)
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$cap = Join-Path $root 'captures'
Get-Process lrc-host,lrc-client -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

$hOut = Join-Path $cap 'wanv-host.out'
$cOut = Join-Path $cap 'wanv-client.out'
foreach ($f in @($hOut, $cOut)) { if (Test-Path $f) { Remove-Item $f -Force } }
$sig = $RelayHost + ':' + $RelayPort

$h = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--password', $Password, '--display', '1', '--stats', '--audio', 'off', '--signal', $sig, '--room', "$Room", '--p2p', 'relay', '--video', 'auto') -RedirectStandardOutput $hOut -RedirectStandardError (Join-Path $cap 'wanv-host.err')
Start-Sleep -Milliseconds 1500
$c = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--password', $Password, '--stats', '--audio', 'off', '--signal', $sig, '--room', "$Room", '--p2p', 'relay', '--title', 'wan-video') -RedirectStandardOutput $cOut -RedirectStandardError (Join-Path $cap 'wanv-client.err')
Start-Sleep -Seconds 3

$pat = Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',(Join-Path $root 'scripts\testpattern3.ps1'),'-TargetX','1920','-Mode','video','-Seconds',"$Seconds")
Start-Sleep -Seconds ($Seconds + 2)
if (-not $pat.HasExited) { Stop-Process -Id $pat.Id -Force }

Stop-Process -Id $c.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $h.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Write-Host "=== client（最后 6 行）==="
Get-Content $cOut -ErrorAction SilentlyContinue | Select-Object -Last 6
Write-Host "=== host（最后 4 行）==="
Get-Content $hOut -ErrorAction SilentlyContinue | Select-Object -Last 4
