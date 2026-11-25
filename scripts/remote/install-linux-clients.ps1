<#
.SYNOPSIS
  在 yanfa2 上安装 lrc 的被控端/控制端（构建 + 安装 + systemd 用户服务）。
.DESCRIPTION
  1) 同步源码并在 yanfa2 上构建 lrc-host-linux / lrc-client-linux
  2) 二进制装到 ~/lrc/bin/
  3) 写 systemd 用户服务（host 常驻，x11vnc 可选）
.EXAMPLE
  powershell -NoProfile -File scripts\remote\install-linux-clients.ps1
  powershell -NoProfile -File scripts\remote\install-linux-clients.ps1 -Password 1234 -Signal 118.24.79.47:7801 -Room 1
#>
param(
    [string]$Password = '1234',
    [string]$Signal   = '118.24.79.47:7801',
    [int]$Room        = 1,
    [string]$Display  = ':1',
    [string]$Auth     = '/run/user/1000/gdm/Xauthority',
    # 只推一台显示器：给 RandR 输出名（例如 DP-1）；留空 = 推整个 X root（双屏会合并）
    [string]$Monitor  = '',
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$vpsKey = 'C:\Users\sss\.ssh\key117.pem'
$remKey = 'C:\Users\sss\.ssh\yanfa2_tunnel'
$vpsProxy = 'ssh -i ' + $vpsKey + ' -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -W %h:%p ubuntu@118.24.79.47'
$base = @('-o', ('ProxyCommand=' + $vpsProxy), '-i', $remKey, '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=NUL', '-o', 'BatchMode=yes')
$sshOpts = $base + @('-p', '2222')
$scpOpts = $base + @('-P', '2222')
$target = 'yanfa2@127.0.0.1'
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'

function Run-Remote([string]$cmd) {
    & ssh @sshOpts $target $cmd
    if ($LASTEXITCODE -ne 0) { Write-Host ('远程命令失败: ' + $cmd) -ForegroundColor Red; return $false }
    return $true
}

if (-not $SkipBuild) {
    Write-Host '=== [1/4] 构建 ===' -ForegroundColor Cyan
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'scripts\remote\linux-build.ps1') -CargoArgs 'build --release -p lrc-linux --bin lrc-host-linux --bin lrc-client-linux'
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prev; throw '构建失败' }
}

Write-Host '=== [2/4] 安装二进制到 ~/lrc/bin ===' -ForegroundColor Cyan
Run-Remote 'mkdir -p ~/lrc/bin && cp ~/lrc-build/target/release/lrc-host-linux ~/lrc-build/target/release/lrc-client-linux ~/lrc/bin/ && ls -la ~/lrc/bin/' | Out-Null

Write-Host '=== [3/4] 写 systemd 用户服务 ===' -ForegroundColor Cyan
$monitorArg = if ($Monitor) { ' --monitor ' + $Monitor } else { '' }
$unitHost = @"
[Unit]
Description=lrc 被控端 (X11 $Display)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
Environment=DISPLAY=$Display
Environment=XAUTHORITY=$Auth
ExecStart=%h/lrc/bin/lrc-host-linux --password $Password --display $Display$monitorArg --stats --listen 127.0.0.1:7800
Restart=always
RestartSec=3
StandardOutput=append:%h/lrc/host.log
StandardError=append:%h/lrc/host.log

[Install]
WantedBy=default.target
"@
$unitHostFile = Join-Path $env:TEMP 'lrc-host-linux.service'
Set-Content -Path $unitHostFile -Value ($unitHost -replace "`r`n", "`n") -NoNewline -Encoding utf8
& scp @scpOpts $unitHostFile ($target + ':/tmp/lrc-host-linux.service')

$unitVnc = @"
[Unit]
Description=x11vnc (lrc 图形通道)
After=graphical-session.target

[Service]
Type=simple
Environment=DISPLAY=$Display
ExecStart=/usr/bin/x11vnc -display $Display -auth $Auth -localhost -forever -shared -rfbport 5900 -noxdamage -nopw -o %h/lrc/x11vnc.log
Restart=always
RestartSec=3

[Install]
WantedBy=default.target
"@
$unitVncFile = Join-Path $env:TEMP 'lrc-x11vnc.service'
Set-Content -Path $unitVncFile -Value ($unitVnc -replace "`r`n", "`n") -NoNewline -Encoding utf8
& scp @scpOpts $unitVncFile ($target + ':/tmp/lrc-x11vnc.service')

Run-Remote 'mkdir -p ~/.config/systemd/user ~/lrc && cp /tmp/lrc-host-linux.service /tmp/lrc-x11vnc.service ~/.config/systemd/user/ && systemctl --user daemon-reload && systemctl --user enable lrc-host-linux lrc-x11vnc && systemctl --user restart lrc-host-linux lrc-x11vnc && sleep 2 && systemctl --user --no-pager status lrc-host-linux | head -8' | Out-Null

Write-Host '=== [4/4] 结果 ===' -ForegroundColor Cyan
Run-Remote 'systemctl --user is-active lrc-host-linux; echo ---; ss -lun | grep 7801 || echo "（7801 是 VPS 的，本机不监听）"; echo ---; tail -6 ~/lrc/host.log 2>/dev/null; echo ---; systemctl --user is-active lrc-x11vnc; ss -tln | grep 5900' | Out-Null

$ErrorActionPreference = $prev
Write-Host '完成。本机连接：powershell -File scripts\remote\connect-yanfa2.ps1' -ForegroundColor Green
