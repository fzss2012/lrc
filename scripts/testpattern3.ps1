<#
.SYNOPSIS
  确定性测试画面（最简实现）：先在内存里把一屏"代码编辑器"画好，再用全屏 PictureBox 显示。
  没有任何 Paint 回调 / 定时器重绘，静止模式下画面绝对不变。
  -Mode static : 只显示，不更新（用于"静止零发送"验收）
  -Mode typing : 每 120ms 在末尾追加一个字符（办公打字近似）
  -Mode scroll : 每 60ms 上移 2px
#>
param(
    [int]$Display = 0,
    # 按"虚拟桌面上的 X 坐标"精确选中一块屏（DXGI 输出序号与 WinForms 屏序不保证一致，用这个更可靠）
    [int]$TargetX = -1,
    [ValidateSet('static', 'typing', 'scroll', 'video')]
    [string]$Mode = 'typing',
    [int]$Seconds = 20,
    [int]$FontSize = 18,
    [string]$RenderOut = ''
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$screens = [System.Windows.Forms.Screen]::AllScreens
if ($TargetX -ge 0) {
    $hit = $screens | Where-Object { $_.Bounds.X -eq $TargetX } | Select-Object -First 1
    if ($null -eq $hit) { throw "找不到 Bounds.X=$TargetX 的显示器" }
    $bounds = $hit.Bounds
} else {
    if ($Display -lt 0 -or $Display -ge $screens.Count) { throw "Display 越界：0..$($screens.Count - 1)" }
    $bounds = $screens[$Display].Bounds
}
$W = [int]$bounds.Width
$H = [int]$bounds.Height

$lines = @(
    'pub fn handle_frame(ctx: &mut Context, frame: Frame) -> Result<()> {',
    '    let grid = Grid::new(64, frame.width, frame.height)?;',
    '    for idx in 0..grid.count() {',
    '        let hash = hash_block(&frame.pixels, frame.stride, idx);',
    '        if ctx.prev[idx] != hash { ctx.changed.push(idx); }',
    '    }',
    '    let packets = encode_changes(&frame, &ctx.changed)?;',
    '    ctx.conn.send(VideoFrame { seq: ctx.seq, ..packets })?;',
    '    Ok(())',
    '}',
    '',
    'fn main() -> Result<()> {',
    '    let mut cap = Capturer::new(0, 16)?;',
    '    let mut ctx = Context::default();',
    '    loop {',
    '        match cap.grab()? {',
    '            None => thread::sleep(Duration::from_millis(2)),',
    '            Some(frame) => handle_frame(&mut ctx, frame)?,',
    '        }',
    '    }',
    '}'
)

$bgColor = [System.Drawing.Color]::FromArgb(30, 30, 32)
$textColor = [System.Drawing.Color]::FromArgb(214, 214, 214)
$noColor = [System.Drawing.Color]::FromArgb(133, 133, 140)
$tmColor = [System.Drawing.Color]::FromArgb(206, 145, 120)
$fontMono = New-Object System.Drawing.Font('Consolas', $FontSize)
$lineH = [int]($FontSize * 1.75)

function New-FrameBitmap([int]$typedLen, [int]$scrollY) {
    $bmp = New-Object System.Drawing.Bitmap($W, $H)
    $gr = [System.Drawing.Graphics]::FromImage($bmp)
    $gr.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
    $bText = New-Object System.Drawing.SolidBrush($textColor)
    $bNo = New-Object System.Drawing.SolidBrush($noColor)
    $bTm = New-Object System.Drawing.SolidBrush($tmColor)
    $gr.Clear($bgColor)
    $y = 16 - $scrollY
    for ($i = 0; $i -lt $lines.Count; $i++) {
        $gr.DrawString(('{0,4}' -f ($i + 1)), $fontMono, $bNo, [float]10, [float]$y)
        $gr.DrawString($lines[$i], $fontMono, $bText, [float]64, [float]$y)
        $y += $lineH
        if ($y -gt $H) { break }
    }
    if ($typedLen -gt 0) {
        $chars = 'abcdefghijklmnopqrstuvwxyz0123456789 ()[]{};,._+=->/*'
        $sb = New-Object System.Text.StringBuilder
        for ($k = 0; $k -lt $typedLen; $k++) { [void]$sb.Append($chars[$k % $chars.Length]) }
        $gr.DrawString($sb.ToString(), $fontMono, $bTm, [float]64, [float]$y)
    }
    $bText.Dispose(); $bNo.Dispose(); $bTm.Dispose()
    $gr.Dispose()
    return $bmp
}

function New-VideoBitmap([int]$phase) {
    $bmp = New-Object System.Drawing.Bitmap($W, $H)
    $gr = [System.Drawing.Graphics]::FromImage($bmp)
    $gr.Clear([System.Drawing.Color]::Black)
    $bands = 40
    $bandH = [int][Math]::Ceiling($H / $bands)
    for ($i = 0; $i -lt $bands; $i++) {
        $y = ($i * $bandH + $phase) % $H
        $t = ($i * 6 + $phase) % 360
        $r = [int](128 + 127 * [Math]::Sin($t * [Math]::PI / 180))
        $g2 = [int](128 + 127 * [Math]::Sin(($t + 120) * [Math]::PI / 180))
        $b2 = [int](128 + 127 * [Math]::Sin(($t + 240) * [Math]::PI / 180))
        $brush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb($r, $g2, $b2))
        # 每层画两段以覆盖环绕
        $gr.FillRectangle($brush, 0, $y, $W, $bandH)
        if ($y + $bandH -gt $H) { $gr.FillRectangle($brush, 0, 0, $W, ($y + $bandH - $H)) }
        $brush.Dispose()
    }
    $gr.Dispose()
    return $bmp
}

$current = if ($Mode -eq 'video') { New-VideoBitmap 0 } else { New-FrameBitmap 0 0 }
if ($RenderOut -ne '') { $current.Save($RenderOut, [System.Drawing.Imaging.ImageFormat]::Png) }

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Bounds = $bounds
$form.TopMost = $true
$form.ShowInTaskbar = $false
$form.BackColor = $bgColor
$pb = New-Object System.Windows.Forms.PictureBox
$pb.Dock = 'Fill'
$pb.SizeMode = 'Normal'
$pb.Image = $current
$form.Controls.Add($pb)
$form.Show()
$form.TopMost = $true
$form.Activate()
[void]$form.Focus()

$frame = 0
$typed = 0
$scroll = 0
$deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
while ([DateTime]::UtcNow -lt $deadline) {
    $frame++
    $doUpdate = $false
    if ($Mode -eq 'typing') {
        if ($frame % 7 -eq 0) { $typed = ($typed + 1) % 80; $doUpdate = $true }
    } elseif ($Mode -eq 'scroll') {
        if ($frame % 4 -eq 0) { $scroll = ($scroll + 2) % 400; $doUpdate = $true }
    } elseif ($Mode -eq 'video') {
        # 整屏运动：横向色带整体滚动 + 纵向明暗呼吸，每帧 ~100% 像素变化
        if ($frame % 2 -eq 0) { $scroll = ($scroll + 7) % 360; $doUpdate = $true }
    }
    if ($doUpdate -and $Mode -eq 'video') {
        $next = New-VideoBitmap $scroll
        $pb.Image = $next
        $pb.Invalidate()
        $old = $current
        $current = $next
        if ($null -ne $old) { $old.Dispose() }
    } elseif ($doUpdate) {
        $next = New-FrameBitmap $typed $scroll
        $pb.Image = $next
        $pb.Invalidate()
        $old = $current
        $current = $next
        if ($null -ne $old) { $old.Dispose() }
    }
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 16
}
$form.Close()
$form.Dispose()
$current.Dispose()
Write-Host "testpattern3 done: display=$Display mode=$Mode seconds=$Seconds"
