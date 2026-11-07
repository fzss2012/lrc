<#
.SYNOPSIS
  确定性测试画面 v2：把画面预渲染成 Bitmap，用全屏 PictureBox 显示（避免 WinForms Paint 抖动）。
  模式：static（完全静止）/ typing（逐字符追加，办公打字近似）/ scroll
#>
param(
    [int]$Display = 0,
    [ValidateSet('static', 'typing', 'scroll')]
    [string]$Mode = 'typing',
    [int]$Seconds = 20,
    [int]$FontSize = 18
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$screens = [System.Windows.Forms.Screen]::AllScreens
if ($Display -lt 0 -or $Display -ge $screens.Count) { throw "Display 越界：0..$($screens.Count - 1)" }
$bounds = $screens[$Display].Bounds
$W = $bounds.Width
$H = $bounds.Height

$codeLines = New-Object System.Collections.ArrayList
foreach ($l in @(
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
)) { [void]$codeLines.Add($l) }

$bgColor = [System.Drawing.Color]::FromArgb(30, 30, 32)
$textColor = [System.Drawing.Color]::FromArgb(214, 214, 214)
$noColor = [System.Drawing.Color]::FromArgb(133, 133, 140)
$tmColor = [System.Drawing.Color]::FromArgb(206, 145, 120)
$font = New-Object System.Drawing.Font('Consolas', $FontSize)
$fontNo = New-Object System.Drawing.Font('Consolas', $FontSize)
$lineH = [int]($FontSize * 1.75)

$bitmap = New-Object System.Drawing.Bitmap($W, $H)
$g = [System.Drawing.Graphics]::FromImage($bitmap)
$g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
$brushText = New-Object System.Drawing.SolidBrush($textColor)
$brushNo = New-Object System.Drawing.SolidBrush($noColor)
$brushTm = New-Object System.Drawing.SolidBrush($tmColor)

function Render([int]$typedLen, [int]$scrollY, [int]$caretOn) {
    $g.Clear($bgColor)
    $y = 16 - $scrollY
    for ($i = 0; $i -lt $codeLines.Count; $i++) {
        $g.DrawString(('{0,4}' -f ($i + 1)), $fontNo, $brushNo, [float]10, [float]$y)
        $g.DrawString($codeLines[$i], $font, $brushText, [float]64, [float]$y)
        $y += $lineH
        if ($y -gt $H) { break }
    }
    if ($typedLen -gt 0) {
        $chars = 'abcdefghijklmnopqrstuvwxyz0123456789 ()[]{};,._+=->/*'
        $s = ''
        for ($k = 0; $k -lt $typedLen; $k++) { $s += $chars[$k % $chars.Length] }
        $g.DrawString($s, $font, $brushTm, [float]64, [float]$y)
    }
    if ($caretOn -ne 0) {
        $g.DrawString('|', $font, $brushText, [float](64 + 9 * ($typedLen % 90)), [float]$y)
    }
}
Render 0 0 1
$g.Dispose()

$bmpPath = 'D:\lrc\captures\pat-render.png'
$bitmap.Save($bmpPath, [System.Drawing.Imaging.ImageFormat]::Png)

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
$pb.Image = $bitmap
$form.Controls.Add($pb)

$form.Show()
$form.Activate()

$frame = 0
$typed = 0
$scroll = 0
$deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
while ([DateTime]::UtcNow -lt $deadline) {
    $frame++
    $dirty = $false
    switch ($Mode) {
        'typing' {
            if ($frame % 4 -eq 0) {
                $typed = ($typed + 1) % 80
                $dirty = $true
            }
            $caret = if (($frame % 40) -lt 24) { 1 } else { 0 }
            if (-not $dirty) { Render $typed $scroll $caret; $dirty = $true }
        }
        'scroll' {
            $scroll = ($scroll + 2) % 400
            $dirty = $true
        }
        'static' {
            if ($frame -eq 1) { Render 0 0 0; $dirty = $true }
        }
    }
    if ($dirty) {
        Render $typed $scroll 0
        $pb.Image = $bitmap
        $pb.Invalidate()
    }
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 16
}
$form.Close()
$form.Dispose()
$bitmap.Dispose()
Write-Host "testpattern2 结束: display=$Display mode=$Mode seconds=$Seconds render=$bmpPath"
