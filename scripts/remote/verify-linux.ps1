# verify-linux.ps1 -- yanfa2 (Ubuntu 24.04 / X11) 端到端验证
#
# 覆盖：
#   1) 环境体检（DISPLAY / 分辨率 / GPU / 磁盘）
#   2) 同步源码 + 远端构建 lrc-linux
#   3) 确定性测试图案 x11-pattern.c：静止格子做像素比对，动画方块做 fps
#   4) 端到端：yanfa2 跑 lrc-host-linux 采 :1，本机 lrc-client.exe 经 SSH 转发连过去
#   5) 断言：fps > 0、decoded_blocks 增长、客户区像素与源一致（8xN 采样点）
#   6) 压力：静止 1080p 时 lrc-host-linux 的 CPU 占用（目标 < 5%）
#
# 示例：
#   powershell -NoProfile -File D:\lrc\scripts\remote\verify-linux.ps1
#   powershell -NoProfile -File D:\lrc\scripts\remote\verify-linux.ps1 -SkipBuild -Seconds 20
#   powershell -NoProfile -File D:\lrc\scripts\remote\verify-linux.ps1 -CpuOnly

param(
    [string]$Display   = ':1',
    [int]$Seconds      = 30,
    [string]$Password  = '1234',
    [int]$LocalPort    = 17800,
    [int]$RemotePort   = 7800,
    [string]$Title     = 'lrc-verify',
    [switch]$SkipBuild,
    [switch]$SkipPixels,
    [switch]$CpuOnly,
    [switch]$KeepRunning,
    [string]$VpsKey    = 'C:\Users\sss\.ssh\key117.pem',
    [string]$RemoteKey = 'C:\Users\sss\.ssh\yanfa2_tunnel',
    [string]$RemoteDir = '/home/yanfa2/lrc-build'
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class LrcWin32 {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
'@

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$cap = Join-Path $root 'captures'
New-Item -ItemType Directory -Force -Path $cap | Out-Null
$results = New-Object System.Collections.ArrayList
$remotePid = $null
$clientProc = $null
$tunnelProc = $null
$hostLog = Join-Path $cap 'linux-host.out'
$cliLog = Join-Path $cap 'linux-client.out'
$screenW = 0
$screenH = 0
$hostReady = $false
$patOk = $false
$xAUTH = ''
foreach ($cand in @('/run/user/1000/gdm/Xauthority', '/home/yanfa2/.Xauthority')) {
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $yn = (& powershell -NoProfile -File (Join-Path $PSScriptRoot 'connect-yanfa2.ps1') -Command ('test -f ' + $cand + ' && echo yes || echo no') 2>$null | Out-String).Trim()
    $ErrorActionPreference = $prevEap
    if ($yn -match 'yes') { $xAUTH = $cand; break }
}
$xenv = 'DISPLAY=' + $Display + ' '
if ($xAUTH -ne '') { $xenv = $xenv + 'XAUTHORITY=' + $xAUTH + ' ' }

function Add-Result([string]$name, [bool]$pass, [string]$detail) {
    [void]$results.Add([pscustomobject]@{ Name = $name; Pass = $pass; Detail = $detail })
    $mark = if ($pass) { 'PASS' } else { 'FAIL' }
    $color = if ($pass) { 'Green' } else { 'Red' }
    Write-Host ('[{0}] {1} -- {2}' -f $mark, $name, $detail) -ForegroundColor $color
}
function Add-Skip([string]$name, [string]$detail) {
    [void]$results.Add([pscustomobject]@{ Name = $name; Pass = $true; Detail = ('SKIP: ' + $detail) })
    Write-Host ('[SKIP] {0} -- {1}' -f $name, $detail) -ForegroundColor Yellow
}
function Remote([string]$cmd, [switch]$AllowFail) {
    $script = Join-Path $root 'scripts\remote\connect-yanfa2.ps1'
    # ssh 会把 "Permanently added ... to known hosts" 之类的告警写到 stderr；
    # 这里临时放宽 ErrorActionPreference，别让告警变成终止错误。
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & powershell -NoProfile -File $script -Command $cmd 2>$null | Out-String
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    if (-not $AllowFail -and $code -ne 0) {
        throw ('远端命令失败(' + $code + '): ' + $cmd + ' :: ' + $out)
    }
    # 去掉 connect-yanfa2.ps1 的说明行/ssh 告警，只留远端命令的真实输出
    $keep = ($out -split '\r?\n' | Where-Object {
        $_ -notmatch '^经 .* 连接 .*（隧道端口' -and
        $_ -notmatch '^powershell.exe : Warning' -and
        $_ -notmatch '^Warning: Permanently added' -and
        $_ -notmatch '^\s*[+-]\s*CategoryInfo' -and
        $_ -notmatch '^\s*[+-]\s*FullyQualifiedErrorId' -and
        $_ -notmatch '^At .*verify-linux.ps1:' -and
        $_ -notmatch '^At line:1 char' -and
        $_ -notmatch '^\+ ~+' -and
        $_ -notmatch '^RemoteException$'
    })
    return ($keep -join "`n")
}
function RemoteLines([string]$cmd) {
    $o = Remote $cmd
    $arr = @($o -split '\r?\n' | Where-Object { $_.Trim() -ne '' })
    Write-Output -NoEnumerate $arr
}
function Stop-RemoteProcs {
    Remote 'pkill -f lrc-host-linux 2>/dev/null; pkill -f x11-pattern 2>/dev/null; true' -AllowFail | Out-Null
    $script:remotePid = $null
}
function Stop-LocalClient {
    Get-Process lrc-client -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    $script:clientProc = $null
}
function Stop-Tunnel {
    if ($script:tunnelProc -and -not $script:tunnelProc.HasExited) {
        Stop-Process -Id $script:tunnelProc.Id -Force -ErrorAction SilentlyContinue
    }
    $script:tunnelProc = $null
}
function Start-Pattern([string]$mode, [int]$secs) {
    $cmd = 'cd ' + $RemoteDir + ' && ' + $xenv + 'nohup ./x11-pattern ' + $mode + ' --seconds ' + $secs + ' > /tmp/pat.log 2>&1 & true'
    Remote $cmd -AllowFail | Out-Null
}
function Start-RemoteHost {
    $cmd = 'cd ' + $RemoteDir + ' && ' + $xenv + 'nohup ./target/release/lrc-host-linux --password ' + $Password + ' --listen 127.0.0.1:' + $RemotePort + ' --stats > /tmp/host.log 2>&1 & true'
    Remote $cmd -AllowFail | Out-Null
    Start-Sleep -Seconds 2
    $script:remotePid = (Remote 'pgrep -f lrc-host-linux | head -1').Trim()
}
function Test-ClientPixels([string]$WindowTitle, [int]$Cols, [int]$Rows, [int]$Cell) {
    $hwnd = [LrcWin32]::FindWindow($null, $WindowTitle)
    if ($hwnd -eq [IntPtr]::Zero) {
        return [pscustomobject]@{ Ok = $false; Detail = ('找不到窗口 ' + $WindowTitle) }
    }
    [void][LrcWin32]::SetForegroundWindow($hwnd)
    Start-Sleep -Milliseconds 400
    $r = New-Object LrcWin32+RECT
    if (-not [LrcWin32]::GetClientRect($hwnd, [ref]$r)) {
        return [pscustomobject]@{ Ok = $false; Detail = 'GetClientRect 失败' }
    }
    $cw = $r.Right - $r.Left
    $ch = $r.Bottom - $r.Top
    if ($cw -lt 32 -or $ch -lt 32) {
        return [pscustomobject]@{ Ok = $false; Detail = ('客户区太小 ' + $cw + 'x' + $ch) }
    }
    $bmp = New-Object System.Drawing.Bitmap($cw, $ch)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [LrcWin32]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    if (-not $ok) {
        $bmp.Dispose()
        return [pscustomobject]@{ Ok = $false; Detail = 'PrintWindow 返回 false' }
    }
    $png = Join-Path $cap 'linux-client-window.png'
    $bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)

    $total = 0; $match = 0; $sumErr = 0.0; $black = 0
    for ($j = 0; $j -lt $Rows; $j++) {
        for ($i = 0; $i -lt $Cols; $i++) {
            $rx = [int](($i + 0.5) * $Cell)
            $ry = [int](($j + 0.5) * $Cell)
            if ($rx -ge $script:screenW -or $ry -ge $script:screenH) { continue }
            $px = [int]($rx * $cw / $script:screenW)
            $py = [int]($ry * $ch / $script:screenH)
            if ($px -lt 0 -or $py -lt 0 -or $px -ge $cw -or $py -ge $ch) { continue }
            $c = $bmp.GetPixel($px, $py)
            if ($c.R -eq 0 -and $c.G -eq 0 -and $c.B -eq 0) { $black++ }
            $er = [math]::Abs([int]$c.R - (($i * 37 + $j * 59) -band 0xFF))
            $eg = [math]::Abs([int]$c.G - (($i * 17 + $j * 23) -band 0xFF))
            $eb = [math]::Abs([int]$c.B - (($i * 71 + $j * 11) -band 0xFF))
            $sumErr += ($er + $eg + $eb) / 3.0
            $total++
            if ($er -le 16 -and $eg -le 16 -and $eb -le 16) { $match++ }
        }
    }
    $bmp.Dispose()
    $mean = if ($total -gt 0) { $sumErr / $total } else { 999 }
    $okp = ($total -gt 0) -and ($match -ge [math]::Ceiling($total * 0.9)) -and ($mean -lt 12)
    $detail = ('{0}/{1} 采样点一致，平均误差 {2:N1}/255，全黑点 {3}，窗口 {4}x{5}' -f $match, $total, $mean, $black, $cw, $ch)
    return [pscustomobject]@{ Ok = $okp; Detail = $detail }
}

try {
    Write-Host '== 1/6 环境体检 ==' -ForegroundColor Cyan
    $sys = RemoteLines 'uname -sr; echo ===; nproc; free -g | sed -n 2p'
    Write-Host ('      ' + ($sys -join ' | '))
    $disp = RemoteLines ('for a in /run/user/1000/gdm/Xauthority ~/.Xauthority; do DISPLAY=' + $Display + ' XAUTHORITY=$a xdpyinfo 2>/dev/null | grep -m1 dimensions && break; done')
    if ($disp.Count -gt 0 -and $disp[0] -match '(\d+)x(\d+)') {
        $script:screenW = [int]$Matches[1]
        $script:screenH = [int]$Matches[2]
        Add-Result 'X11 会话' $true ($Display + ' ' + $script:screenW + 'x' + $script:screenH)
    } else {
        Add-Result 'X11 会话' $false ($Display + ' 打不开（xdpyinfo 无输出）')
    }
    $gpu = (Remote 'nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -1').Trim()
    Add-Result 'GPU' ($gpu -ne '') $gpu
    $df = (Remote ('df -h ' + $RemoteDir + ' 2>/dev/null | tail -1')).Trim()
    Write-Host ('      磁盘: ' + $df)
    $used = 0
    if ($df -match '(\d+)%') { $used = [int]$Matches[1] }
    if ($used -ge 99) { Add-Result '磁盘余量' $false $df }
    elseif ($used -ge 95) { Add-Skip '磁盘余量' ('磁盘快满了（' + $df + '），注意构建会失败') }
    else { Add-Result '磁盘余量' $true $df }

    Write-Host '== 2/6 同步 + 构建 ==' -ForegroundColor Cyan
    if ($SkipBuild) {
        Add-Skip '远端构建' '按 -SkipBuild 跳过'
    } else {
        $buildLog = Join-Path $cap 'linux-build.out'
        $prevEap2 = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        & powershell -NoProfile -File (Join-Path $root 'scripts\remote\linux-build.ps1') -CargoArgs 'build --release -p lrc-linux --bin lrc-host-linux --bin x11-probe' 2>&1 |
            Tee-Object -FilePath $buildLog | Select-Object -Last 5 | ForEach-Object { Write-Host ('      ' + $_) -ForegroundColor Gray }
        $buildText = (Get-Content $buildLog -Raw -ErrorAction SilentlyContinue)
        $buildOk = ($LASTEXITCODE -eq 0) -and -not ($buildText -match 'error\[|error: could not compile')
        $ErrorActionPreference = $prevEap2
        Add-Result '远端构建' $buildOk ($(if ($buildOk) { 'cargo build --release -p lrc-linux OK' } else { '见 ' + $buildLog }))
        if (-not $buildOk) { throw '构建失败，后续跳过' }
    }
    $found = @()
    foreach ($b in @('lrc-host-linux', 'lrc-client-linux', 'x11-probe')) {
        $t = (Remote ('test -x ' + $RemoteDir + '/target/release/' + $b + ' && echo yes || echo no')).Trim()
        if ($t -eq 'yes') { $found += $b }
    }
    Add-Result '二进制产出' ($found.Count -gt 0) ('[' + ($found -join ', ') + ']')
    if ($found -contains 'lrc-host-linux') {
        $probe = ((Remote ('timeout 8 ' + $RemoteDir + '/target/release/lrc-host-linux --help 2>&1 | head -8')) -replace '\s+', ' ').Trim()
        $script:hostReady = -not ($probe -match '待实现')
        Add-Result 'lrc-host-linux 可用' $script:hostReady ($(if ($script:hostReady) { $probe } else { '还是 stub: ' + $probe }))
    } else {
        Add-Result 'lrc-host-linux 可用' $false '没产出（host-app 未交付）'
    }

    Write-Host '== 3/6 测试图案 ==' -ForegroundColor Cyan
    $vpsProxy = 'ssh -i ' + $VpsKey + ' -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -W %h:%p ubuntu@118.24.79.47'
    $scpBase = @('-o', ('ProxyCommand=' + $vpsProxy), '-i', $RemoteKey, '-o', 'StrictHostKeyChecking=no',
                 '-o', 'UserKnownHostsFile=NUL', '-o', 'BatchMode=yes', '-P', '2222')
    $patLocal = Join-Path $env:TEMP 'x11-pattern.c'
    Copy-Item (Join-Path $root 'scripts\remote\x11-pattern.c') $patLocal -Force
    $prevEap3 = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & scp @scpBase $patLocal ('yanfa2@127.0.0.1:' + $RemoteDir + '/x11-pattern.c') 2>&1 | Out-Null
    $ErrorActionPreference = $prevEap3
    $cc = Remote ('cd ' + $RemoteDir + ' && (gcc -O2 -o x11-pattern x11-pattern.c -lX11 2>&1 || echo NEED_PKG)') -join ' '
    if ($cc -match 'NEED_PKG') {
        Write-Host '      缺 gcc/libX11，尝试 apt 安装...' -ForegroundColor Yellow
        Remote 'sudo apt-get install -y gcc libx11-dev >/dev/null 2>&1; true' -AllowFail | Out-Null
        $cc = Remote ('cd ' + $RemoteDir + ' && gcc -O2 -o x11-pattern x11-pattern.c -lX11 2>&1') -join ' '
    }
    $script:patOk = ((Remote ('test -x ' + $RemoteDir + '/x11-pattern && echo yes || echo no')).Trim() -eq 'yes')
    Add-Result '测试图案编译' $script:patOk ($(if ($script:patOk) { $RemoteDir + '/x11-pattern' } else { $cc }))
    $cell = 0
    if ($script:screenW -gt 0) { $cell = [int]($script:screenW / 8) }

    Write-Host '== 4/6 静止 CPU 占用 ==' -ForegroundColor Cyan
    if (-not $script:hostReady) {
        Add-Skip '静止 CPU (<5%)' 'lrc-host-linux 待交付'
    } elseif (-not $script:patOk) {
        Add-Skip '静止 CPU (<5%)' '图案程序不可用'
    } else {
        Stop-RemoteProcs
        Start-Pattern '--static' 60
        Start-RemoteHost
        if ($script:remotePid -match '^\d+$') {
            $t1 = (Remote ('awk ''{print $14+$15}'' /proc/' + $script:remotePid + '/stat')).Trim()
            Start-Sleep -Seconds 6
            $t2 = (Remote ('awk ''{print $14+$15}'' /proc/' + $script:remotePid + '/stat')).Trim()
            if (($t1 -match '^\d+$') -and ($t2 -match '^\d+$')) {
                $cpu = (([int]$t2 - [int]$t1) / 100.0 / 6.0) * 100.0
                $stat = ((Remote 'tail -1 /tmp/host.log 2>/dev/null') -replace '\s+', ' ').Trim()
                $mp = ($script:screenW * $script:screenH) / (1920.0 * 1080.0)
                $cpu1080 = $cpu / [math]::Max($mp, 0.1)
                Add-Result '静止 CPU (<5% @1080p)' ($cpu1080 -lt 5.0) ('实测 {0:N2}% @ {1}x{2}（折合 1080p {3:N2}%）| {4}' -f $cpu, $script:screenW, $script:screenH, $cpu1080, $stat)
            } else {
                Add-Result '静止 CPU (<5%)' $false ('读不到 /proc/' + $script:remotePid + '/stat')
            }
        } else {
            $tail = ((Remote 'tail -5 /tmp/host.log 2>/dev/null') -replace '\s+', ' ').Trim()
            Add-Result '静止 CPU (<5%)' $false ('host 没起来: ' + $tail)
        }
        Remote 'pkill -f x11-pattern 2>/dev/null; true' -AllowFail | Out-Null
        Stop-RemoteProcs
    }

    Write-Host '== 5/6 端到端 ==' -ForegroundColor Cyan
    $e2eOk = $false
    if ($CpuOnly) {
        Add-Skip '端到端' '按 -CpuOnly 跳过'
    } elseif (-not $script:hostReady) {
        Add-Skip '端到端' 'lrc-host-linux 待交付'
    } else {
        $fwdArgs = @('-o', ('ProxyCommand=' + $vpsProxy), '-i', $RemoteKey, '-o', 'StrictHostKeyChecking=no',
                     '-o', 'UserKnownHostsFile=NUL', '-o', 'BatchMode=yes', '-p', '2222', '-N',
                     '-L', ($LocalPort.ToString() + ':127.0.0.1:' + $RemotePort), 'yanfa2@127.0.0.1')
        $script:tunnelProc = Start-Process -FilePath 'ssh' -ArgumentList $fwdArgs -PassThru -WindowStyle Hidden
        Start-Sleep -Seconds 2
        Add-Result 'SSH 端口转发' (-not $script:tunnelProc.HasExited) ('127.0.0.1:' + $LocalPort + ' -> yanfa2:' + $RemotePort)

        Stop-RemoteProcs
        Start-Pattern '--anim' ($Seconds + 25)
        Start-RemoteHost

        foreach ($f in @($hostLog, $cliLog)) { if (Test-Path $f) { Remove-Item $f -Force } }
        $clientExe = Join-Path $root 'target\release\lrc-client.exe'
        if (-not (Test-Path $clientExe)) { throw ('没有 ' + $clientExe + '，先 cargo build --release') }
        $clientArgs = @('--connect', ('127.0.0.1:' + $LocalPort), '--password', $Password, '--title', $Title, '--stats', '--audio', 'off')
        $script:clientProc = Start-Process -FilePath $clientExe -PassThru -WindowStyle Normal -RedirectStandardOutput $cliLog -RedirectStandardError (Join-Path $cap 'linux-client.err') -ArgumentList $clientArgs
        Add-Result '本机 client 启动' (-not $script:clientProc.HasExited) ($clientExe + ' --connect 127.0.0.1:' + $LocalPort)

        $connected = $false
        for ($i = 0; $i -lt 40; $i++) {
            Start-Sleep -Milliseconds 500
            if (Test-Path $cliLog) {
                $txt = Get-Content $cliLog -Raw -ErrorAction SilentlyContinue
                if ($txt -match 'decoded_blocks=([1-9]\d*)') { $connected = $true; break }
            }
            if ($script:clientProc.HasExited) { break }
        }
        Add-Result '收流建立' $connected ($(if ($connected) { '客户端已解码块' } else { '20 秒内没有 decoded_blocks' }))

        if ($connected) {
            $samples = New-Object System.Collections.ArrayList
            $end = (Get-Date).AddSeconds($Seconds)
            while ((Get-Date) -lt $end) {
                Start-Sleep -Seconds 2
                if (Test-Path $cliLog) {
                    $last = Get-Content $cliLog | Select-Object -Last 1
                    if ($last -match 'fps=(\d+) recv_kbps=([\d.]+) decoded_blocks=(\d+)') {
                        [void]$samples.Add([pscustomobject]@{ fps = [int]$Matches[1]; kbps = [double]$Matches[2]; blocks = [int]$Matches[3] })
                    }
                }
            }
            if ($samples.Count -gt 0) {
                $maxFps = ($samples | Measure-Object -Property fps -Maximum).Maximum
                $maxB = ($samples | Measure-Object -Property blocks -Maximum).Maximum
                $minB = ($samples | Measure-Object -Property blocks -Minimum).Minimum
                $avgK = [math]::Round((($samples | Measure-Object -Property kbps -Average).Average), 1)
                Add-Result 'fps > 0' ($maxFps -gt 0) ('max fps=' + $maxFps + '，平均 ' + $avgK + ' kbps，采样 ' + $samples.Count + ' 次')
                Add-Result 'decoded_blocks 增长' ($maxB -gt $minB) ('blocks ' + $minB + ' -> ' + $maxB)
            } else {
                Add-Result 'fps > 0' $false '客户端 --stats 没有可用行'
            }

            if (-not $SkipPixels -and $script:patOk -and $cell -gt 0) {
                Remote 'pkill -f x11-pattern 2>/dev/null; true' -AllowFail | Out-Null
                Start-Pattern '--static' 40
                Start-Sleep -Seconds 3
                $rows = [int][math]::Ceiling($script:screenH / [double]$cell)
                if ($rows -gt 8) { $rows = 8 }
                $pix = Test-ClientPixels -WindowTitle $Title -Cols 8 -Rows $rows -Cell $cell
                Add-Result '像素一致（8xN 采样）' $pix.Ok $pix.Detail
                $e2eOk = $pix.Ok
            } else {
                Add-Skip '像素一致' '按 -SkipPixels / 图案不可用 / 分辨率未知'
            }
        }
    }

    Write-Host '== 6/6 汇总 ==' -ForegroundColor Cyan
    $fail = ($results | Where-Object { -not $_.Pass }).Count
    $results | Format-Table -AutoSize | Out-String | Write-Host
    if ($e2eOk) { Write-Host '端到端：PASS' -ForegroundColor Green }
    elseif (-not $script:hostReady) { Write-Host '端到端：待交付（lrc-host-linux 还是 stub）' -ForegroundColor Yellow }
    else { Write-Host '端到端：FAIL' -ForegroundColor Red }
    if ($fail -gt 0) { exit 1 } else { exit 0 }
}
finally {
    if (-not $KeepRunning) {
        Stop-LocalClient
        Stop-Tunnel
        Stop-RemoteProcs
    }
}
