<#
.SYNOPSIS
  M4 验收：P2P 打洞（应有 path=direct_udp）+ 强制 relay（应有 path=relay_udp 且 relay 有转发字节）。
.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File D:\lrc\scripts\verify-p2p.ps1
#>
param(
    [int]$RelayPort = 7801,
    # relay 地址：本机自测填 127.0.0.1（脚本会自己起 relay）；跨公网填 VPS 公网 IP（脚本不起本地 relay）
    [string]$RelayHost = "127.0.0.1",
    [int]$Room = 1,
    [int]$HostDisplay = 1,
    [int]$PatternTargetX = 1920,
    [string]$Password = "1234",
    [int]$Seconds = 8
)
$ErrorActionPreference = 'Stop'
$root = 'D:\lrc'
$cap = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $cap | Out-Null

function Stop-All {
    Get-Process lrc-host,lrc-client,lrc-relay,udp-proxy -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 400
}

function Run-Phase([string]$p2p, [string]$tag) {
    Stop-All
    $relayOut = Join-Path $cap "p2p-$tag-relay.out"
    $hostOut = Join-Path $cap "p2p-$tag-host.out"
    $clientOut = Join-Path $cap "p2p-$tag-client.out"
    foreach ($f in @($relayOut, $hostOut, $clientOut)) { if (Test-Path $f) { Remove-Item $f -Force } }

    Write-Host ("== 阶段 [{0}] p2p={1} ==" -f $tag, $p2p) -ForegroundColor Cyan
    $relay = $null
    if ($RelayHost -eq '127.0.0.1' -or $RelayHost -eq 'localhost') {
        $relay = Start-Process -FilePath (Join-Path $root 'target\release\lrc-relay.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--udp', "127.0.0.1:$RelayPort", '--stats') -RedirectStandardOutput $relayOut -RedirectStandardError (Join-Path $cap "p2p-$tag-relay.err")
        Start-Sleep -Milliseconds 800
    } else {
        Write-Host ("  使用远端 relay {0}:{1}（本地不起 relay）" -f $RelayHost, $RelayPort)
        Set-Content -Path $relayOut -Value '' -Encoding UTF8
    }

    $signalAddr = $RelayHost + ':' + $RelayPort
    $hostProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-host.exe') -PassThru -WindowStyle Hidden -ArgumentList @('--password', $Password, '--display', "$HostDisplay", '--stats', '--audio', 'off', '--signal', $signalAddr, '--room', "$Room", '--p2p', $p2p) -RedirectStandardOutput $hostOut -RedirectStandardError (Join-Path $cap "p2p-$tag-host.err")
    Start-Sleep -Milliseconds 1200
    # --connect 仍要给（TCP 兜底地址），但 P2P 成功时不会用到
    $clientProc = Start-Process -FilePath (Join-Path $root 'target\release\lrc-client.exe') -PassThru -ArgumentList @('--connect', "127.0.0.1:7800", '--password', $Password, '--stats', '--audio', 'off', '--signal', $signalAddr, '--room', "$Room", '--p2p', $p2p, '--title', "p2p-$tag") -RedirectStandardOutput $clientOut -RedirectStandardError (Join-Path $cap "p2p-$tag-client.err")

    Start-Sleep -Seconds $Seconds
    Stop-All
    Start-Sleep -Milliseconds 300

    $hLines = @(Get-Content $hostOut -ErrorAction SilentlyContinue | Where-Object { $_ -match 'path=' })
    $cLines = @(Get-Content $clientOut -ErrorAction SilentlyContinue | Where-Object { $_ -match 'path=' })
    $rLines = @(Get-Content $relayOut -ErrorAction SilentlyContinue)
    $hPath = if ($hLines.Count) { ($hLines[-1] -replace '.*path=([a-z_]+).*', '$1') } else { 'NONE' }
    $cPath = if ($cLines.Count) { ($cLines[-1] -replace '.*path=([a-z_]+).*', '$1') } else { 'NONE' }
    $relayed = 0
    foreach ($l in $rLines) { if ($l -match 'relayed_bytes=(\d+)') { $relayed = [int]$Matches[1] } }
    Write-Host ("   host path={0}  client path={1}  relay relayed_bytes={2}" -f $hPath, $cPath, $relayed)
    return [pscustomobject]@{ Tag = $tag; HostPath = $hPath; ClientPath = $cPath; RelayedBytes = $relayed; HostOut = $hostOut; ClientOut = $clientOut; RelayOut = $relayOut }
}

$results = @()
$results += Run-Phase 'auto' 'auto'
$results += Run-Phase 'relay' 'relay'

Write-Host ""
Write-Host "================ M4 汇总 ================" -ForegroundColor Cyan
$ok1 = ($results[0].HostPath -eq 'direct_udp') -and ($results[0].ClientPath -eq 'direct_udp')
$ok2 = ($results[1].HostPath -eq 'relay_udp') -and ($results[1].ClientPath -eq 'relay_udp') -and ($results[1].RelayedBytes -gt 0)
Write-Host ("[P2P 打洞]     期望 direct_udp/direct_udp，实际 {0}/{1}  =>  {2}" -f $results[0].HostPath, $results[0].ClientPath, $(if ($ok1) { 'PASS' } else { 'FAIL' })) -ForegroundColor $(if ($ok1) { 'Green' } else { 'Red' })
Write-Host ("[强制 relay]   期望 relay_udp 且 relay 有转发，实际 {0}/{1} relayed={2}  =>  {3}" -f $results[1].HostPath, $results[1].ClientPath, $results[1].RelayedBytes, $(if ($ok2) { 'PASS' } else { 'FAIL' })) -ForegroundColor $(if ($ok2) { 'Green' } else { 'Red' })
Write-Host ""
Write-Host ("日志目录: {0}" -f $cap)
