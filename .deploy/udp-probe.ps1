# 从 Windows 向 VPS 打 UDP 包，验证入站 7801 是否放行
param([string]$TargetIp = '118.24.79.47', [int]$Port = 7801, [int]$Count = 5)
$udp = New-Object System.Net.Sockets.UdpClient
$payload = [System.Text.Encoding]::ASCII.GetBytes('hello-lrc')
for ($i = 1; $i -le $Count; $i++) {
    [void]$udp.Send($payload, $payload.Length, $TargetIp, $Port)
    Start-Sleep -Milliseconds 200
}
$udp.Close()
Write-Host ("sent {0} UDP datagrams -> {1}:{2}" -f $Count, $TargetIp, $Port)
