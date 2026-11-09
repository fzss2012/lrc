<#
.SYNOPSIS
  同步 D:\lrc 源码到 yanfa2 并在那边跑 cargo（构建/测试）。
.EXAMPLE
  powershell -NoProfile -File scripts\remote\linux-build.ps1
  powershell -NoProfile -File scripts\remote\linux-build.ps1 -CargoArgs "test -p lrc-linux"
  powershell -NoProfile -File scripts\remote\linux-build.ps1 -CargoArgs "run --release -p lrc-linux --bin x11-probe"
#>
param(
    [string]$CargoArgs = 'build --release -p lrc-linux',
    [switch]$NoSync,
    [string]$VpsKey     = 'C:\Users\sss\.ssh\key117.pem',
    [string]$RemoteKey  = 'C:\Users\sss\.ssh\yanfa2_tunnel',
    [string]$RemoteDir  = '/home/yanfa2/lrc-build'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$vpsProxy = 'ssh -i ' + $VpsKey + ' -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -W %h:%p ubuntu@118.24.79.47'
$base = @('-o', ('ProxyCommand=' + $vpsProxy), '-i', $RemoteKey, '-o', 'StrictHostKeyChecking=no', '-o', 'UserKnownHostsFile=NUL', '-o', 'BatchMode=yes')
$sshOpts = $base + @('-p', '2222')
$scpOpts = $base + @('-P', '2222')
$target = 'yanfa2@127.0.0.1'
$prev = $ErrorActionPreference
$ErrorActionPreference = 'Continue'

if (-not $NoSync) {
    Write-Host '[1/3] 打包源码...' -ForegroundColor Cyan
    $tar = Join-Path $env:TEMP 'lrc-src.tar.gz'
    if (Test-Path $tar) { Remove-Item $tar -Force }
    Push-Location $root
    tar -czf $tar --exclude=./target --exclude=./.git --exclude=./captures --exclude=./.deploy Cargo.toml Cargo.lock crates docs scripts
    Pop-Location
    Write-Host ('      {0} KB' -f [math]::Round((Get-Item $tar).Length / 1KB, 1)) -ForegroundColor Gray

    Write-Host '[2/3] 上传并解压...' -ForegroundColor Cyan
    & scp @scpOpts $tar ($target + ':/tmp/lrc-src.tar.gz')
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prev; throw 'scp 失败' }
    & ssh @sshOpts $target ('mkdir -p ' + $RemoteDir + ' && tar -xzf /tmp/lrc-src.tar.gz -C ' + $RemoteDir)
    if ($LASTEXITCODE -ne 0) { $ErrorActionPreference = $prev; throw '解压失败' }
}

Write-Host ('[3/3] 远端: cargo ' + $CargoArgs) -ForegroundColor Cyan
$remoteCmd = 'cd ' + $RemoteDir + ' && ~/build.sh ' + $CargoArgs + ' 2>&1 | tail -80'
& ssh @sshOpts $target $remoteCmd
$code = $LASTEXITCODE
$ErrorActionPreference = $prev
Write-Host ('exit = ' + $code) -ForegroundColor $(if ($code -eq 0) { 'Green' } else { 'Red' })
exit $code
