<#
.SYNOPSIS
  在本机通过共用 VPS 跳到 yanfa2 的 SSH（yanfa2 上需先跑 yanfa2-tunnel.sh）。
.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\remote\connect-yanfa2.ps1
  powershell -NoProfile -File D:\lrc\scripts\remote\connect-yanfa2.ps1 -Command "uptime; nvidia-smi"
#>
param(
    [string]$VpsUser = 'ubuntu',
    [string]$VpsHost = '118.24.79.47',
    [string]$VpsKey = 'C:\Users\sss\.ssh\key117.pem',
    [int]$TunnelPort = 2222,
    [string]$RemoteUser = 'yanfa2',
    [string]$RemoteKey = '',
    [string]$Command = ''
)
$ErrorActionPreference = 'Stop'
$jump = "$VpsUser@$VpsHost"
$target = "$RemoteUser@127.0.0.1"
$sshArgs = @('-J', $jump, '-p', "$TunnelPort", '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=NUL')
if ($VpsKey -ne '') { $sshArgs = @('-i', $VpsKey) + $sshArgs }
if ($RemoteKey -ne '') { $sshArgs += @('-i', $RemoteKey) }
$sshArgs += $target
if ($Command -ne '') { $sshArgs += $Command }

Write-Host ("经 {0} 连接 {1}（隧道端口 {2}）..." -f $jump, $target, $TunnelPort) -ForegroundColor Cyan
& ssh @sshArgs
exit $LASTEXITCODE
