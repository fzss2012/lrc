<#
.SYNOPSIS
  在 Windows 上打包"只够构建 lrc-relay 的最小源码树"，用于上传到 Linux VPS。
.DESCRIPTION
  lrc-relay 只依赖 lrc-proto / lrc-udp（后者只在 Windows 下拉 windows crate），
  所以不需要把 host/client/video/audio 那些 Windows 专用 crate 传上去。
.EXAMPLE
  powershell -NoProfile -File D:\lrc\scripts\deploy\package-relay-src.ps1
  # 产出：D:\lrc\.deploy\relay-src.tar.gz
#>
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$deploy = Join-Path $root '.deploy'
$stage = Join-Path $deploy 'relay-src'

if (Test-Path $deploy) { Remove-Item $deploy -Recurse -Force }
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'crates') | Out-Null
foreach ($c in @('lrc-proto', 'lrc-udp', 'lrc-relay')) {
    Copy-Item (Join-Path $root "crates\$c") -Destination (Join-Path $stage 'crates') -Recurse
}
Copy-Item (Join-Path $root 'Cargo.lock') -Destination $stage

# 只保留 3 个 member 的 workspace 清单
$toml = Get-Content (Join-Path $root 'Cargo.toml') -Raw
$trimmed = [regex]::Replace($toml, '(?s)members = \[.*?\]', @"
members = [
    "crates/lrc-proto",
    "crates/lrc-udp",
    "crates/lrc-relay",
]
"@)
Set-Content -Path (Join-Path $stage 'Cargo.toml') -Value $trimmed -Encoding UTF8

$tarPath = Join-Path $deploy 'relay-src.tar.gz'
if (Test-Path $tarPath) { Remove-Item $tarPath -Force }
tar -czf $tarPath -C $deploy relay-src
$size = [math]::Round((Get-Item $tarPath).Length / 1KB, 1)
Write-Host "已生成: $tarPath ($size KB)"
Write-Host ""
Write-Host "上传并构建:"
Write-Host "  scp -i C:\Users\sss\.ssh\key117.pem $tarPath ubuntu@<VPS_IP>:/tmp/"
Write-Host "  ssh -i C:\Users\sss\.ssh\key117.pem ubuntu@<VPS_IP> 'mkdir -p ~/lrc && tar -xzf /tmp/relay-src.tar.gz -C ~/lrc'"
