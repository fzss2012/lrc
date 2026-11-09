<#
.SYNOPSIS
  在本机通过共用 VPS 跳到 yanfa2（yanfa2 上需先跑 yanfa2-tunnel.sh）。
.DESCRIPTION
  链路：本机 --ssh--> VPS --127.0.0.1:2222(反向隧道)--> yanfa2:22
  图形界面（可选）：-Vnc 会在本地开 5900，转发到 VPS 回环的 5900 → yanfa2 的 x11vnc。
.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\remote\connect-yanfa2.ps1
  powershell -NoProfile -File D:\lrc\scripts\remote\connect-yanfa2.ps1 -Command "uptime; nvidia-smi"
  powershell -NoProfile -File D:\lrc\scripts\remote\connect-yanfa2.ps1 -Vnc
#>
param(
    [string]$VpsUser    = 'ubuntu',
    [string]$VpsHost    = '118.24.79.47',
    [string]$VpsKey     = 'C:\Users\sss\.ssh\key117.pem',
    [int]$TunnelPort    = 2222,
    [string]$RemoteUser = 'yanfa2',
    [string]$RemoteKey  = 'C:\Users\sss\.ssh\yanfa2_tunnel',
    [string]$Command    = '',
    [switch]$Vnc
)
$ErrorActionPreference = 'Stop'

$proxyCmd = 'ssh -i "' + $VpsKey + '" -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -W %h:%p ' + $VpsUser + '@' + $VpsHost
$target = $RemoteUser + '@127.0.0.1'

if ($Vnc) {
    Write-Host ('建立 VNC 转发：本机 127.0.0.1:5900 -> ' + $VpsHost + ' 回环 5900 -> yanfa2:5900') -ForegroundColor Cyan
    Write-Host '保持这个窗口开着，然后用 VNC 客户端连 127.0.0.1:5900（Ctrl-C 断开）' -ForegroundColor Yellow
    & ssh -o ('ProxyCommand=' + $proxyCmd) -i $RemoteKey -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -p $TunnelPort -N -L '5900:127.0.0.1:5900' $target
    exit $LASTEXITCODE
}

$sshArgs = @('-o', ('ProxyCommand=' + $proxyCmd), '-i', $RemoteKey,
             '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=NUL',
             '-o', 'BatchMode=yes', '-p', "$TunnelPort", $target)
if ($Command -ne '') { $sshArgs += $Command }

Write-Host ('经 ' + $VpsHost + ' 连接 ' + $target + '（隧道端口 ' + $TunnelPort + '）...') -ForegroundColor Cyan
& ssh @sshArgs
exit $LASTEXITCODE
