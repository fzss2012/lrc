# 测 relay 放大率：应用层字节 vs relay 转发字节
param([string]$RelayHost = '118.24.79.47', [int]$RelayPort = 7801, [int]$Room = 31,
      [string]$Password = '1234', [int]$Seconds = 12,
      [string]$SshTarget = 'ubuntu@118.24.79.47',
      [string]$SshKey = 'C:\Users\sss\.ssh\key117.pem')
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$cap = Join-Path $root 'captures'
Get-Process lrc-host,lrc-client -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
$hOut = Join-Path $cap 'amp-host.out'
$cOut = Join-Path $cap 'amp-client.out'
foreach ($f in @($hOut, $cOut)) { if (Test-Path $f) { Remove-Item $f -Force } }
$sig = $RelayHost + ':' + $RelayPort

function Get-Relayed {
  $prev = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  $line = & ssh -i $SshKey -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes $SshTarget 'tail -1 ~/relay.log' 2>$null
  $ErrorActionPreference = $prev
  if ($line -match 'relayed_bytes=(\d+)') { return [long]$Matches[1] }
  return -1
}

$before = Get-Relayed
$h = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--password', $Password, '--display', '1', '--stats', '--audio', 'off', '--signal', $sig, '--room', "$Room", '--p2p', 'relay', '--video', 'off') -RedirectStandardOutput $hOut -RedirectStandardError (Join-Path $cap 'amp-host.err')
Start-Sleep -Milliseconds 1500
$c = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--password', $Password, '--stats', '--audio', 'off', '--signal', $sig, '--room', "$Room", '--p2p', 'relay', '--title', 'amp') -RedirectStandardOutput $cOut -RedirectStandardError (Join-Path $cap 'amp-client.err')
# 用打字图案产生稳定但不过量的流量
$pat = Start-Process -FilePath 'powershell.exe' -PassThru -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',(Join-Path $root 'scripts\testpattern3.ps1'),'-TargetX','1920','-Mode','typing','-Seconds',"$Seconds")
Start-Sleep -Seconds ($Seconds + 2)
if (-not $pat.HasExited) { Stop-Process -Id $pat.Id -Force }
$after = Get-Relayed
Start-Sleep -Milliseconds 300
Stop-Process -Id $c.Id -Force -ErrorAction SilentlyContinue
Stop-Process -Id $h.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

$hLast = Get-Content $hOut | Select-Object -Last 1
$cLast = Get-Content $cOut | Select-Object -Last 1
$appBytes = 0
if ($hLast -match 'bytes=(\d+)') { $appBytes = [long]$Matches[1] }
$relayDelta = $after - $before
Write-Host "=== host 最后一行 ==="
Write-Host $hLast
Write-Host "=== client 最后一行 ==="
Write-Host $cLast
Write-Host ""
Write-Host ("relay 转发增量 = {0} 字节" -f $relayDelta)
Write-Host ("host 应用层累计   = {0} 字节" -f $appBytes)
if ($appBytes -gt 0) {
  Write-Host ("放大率 = {0:N2}x" -f ($relayDelta / $appBytes))
} else { Write-Host "无法计算放大率" }
