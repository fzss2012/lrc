<#
.SYNOPSIS
  确定性测试画面：在指定显示器上全屏显示一个"代码编辑器"风格的窗口，
  用来做可重复的带宽/画质基准（静止 = 0 字节，typing = 办公打字场景）。

.EXAMPLE
  # 在 DISPLAY1 上模拟连续打字 20 秒
  pwsh -File D:\lrc\scripts\testpattern.ps1 -Display 0 -Mode typing -Seconds 20

  # 静态画面（用于验证"静止零发送"）
  pwsh -File D:\lrc\scripts\testpattern.ps1 -Display 0 -Mode static -Seconds 10

  # 滚动（网页滚动近似）
  pwsh -File D:\lrc\scripts\testpattern.ps1 -Display 0 -Mode scroll -Seconds 20
#>
param(
    [int]$Display = 0,
    [ValidateSet('static', 'typing', 'scroll', 'noise')]
    [string]$Mode = 'typing',
    [int]$Seconds = 20,
    [int]$FontSize = 16
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

$screens = [System.Windows.Forms.Screen]::AllScreens
if ($Display -lt 0 -or $Display -ge $screens.Count) { throw "Display 越界：可选 0..$($screens.Count - 1)" }
$bounds = $screens[$Display].Bounds

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'
$form.StartPosition = 'Manual'
$form.Bounds = $bounds
$form.TopMost = $true
$form.BackColor = [System.Drawing.Color]::FromArgb(30, 30, 30)
$form.ShowInTaskbar = $false

$state = @{
    Typed = ''
    ScrollY = 0
    Frame = 0
    Rnd = New-Object System.Random(12345)
    Pixels = $null
}

$codeLines = @(
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
    '    loop {',
    '        match cap.grab()? {',
    '            None => thread::sleep(Duration::from_millis(2)),',
    '            Some(frame) => handle_frame(&mut ctx, frame)?,',
    '        }',
    '    }',
    '}'
)

$paint = {
    param($sender, $e)
    $g = $e.Graphics
    $g.Clear([System.Drawing.Color]::FromArgb(30, 30, 30))
    $mono = New-Object System.Drawing.Font('Consolas', $FontSize)
    $lineNoFont = New-Object System.Drawing.Font('Consolas', $FontSize)
    $brushText = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(212, 212, 212))
    $brushNo = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(133, 133, 133))
    $brushStr = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(206, 145, 120))
    $bg = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(30, 30, 30))
    $g.FillRectangle($bg, 0, 0, $bounds.Width, $bounds.Height)
    $lineH = [int]($FontSize * 1.8)
    $y = 12 - $state.ScrollY
    $i = 0
    foreach ($line in $codeLines) {
        $ln = ('{0,4}' -f ($i + 1))
        $g.DrawString($ln, $lineNoFont, $brushNo, [float]6, [float]$y)
        $g.DrawString($line, $mono, $brushText, [float]56, [float]$y)
        $y += $lineH
        $i++
        if ($y -gt $bounds.Height) { break }
    }
    if ($state.Typed -ne '') {
        $g.DrawString($state.Typed, $mono, $brushStr, [float]56, [float]$y)
    }
    # 光标（闪烁）
    if (($state.Frame % 30) -lt 18 -and $state.Typed -eq '') {
        $g.DrawString('|', $mono, $brushText, [float]56, [float]$y)
    }
    if ($Mode -eq 'noise') {
        # 全屏噪点，模拟视频/游戏画面（有损编码的压力测试）
        $bmp = $state.Pixels
        if ($null -ne $bmp) {
            $g.DrawImageUnscaled($bmp, 0, 0)
        }
    }
    $mono.Dispose(); $lineNoFont.Dispose()
    $brushText.Dispose(); $brushNo.Dispose(); $brushStr.Dispose(); $bg.Dispose()
}

$form.add_Paint($paint)

$chars = 'abcdefghijklmnopqrstuvwxyz0123456789 ()[]{};,._+=->/*'
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 16
$timer.add_Tick({
    $state.Frame = $state.Frame + 1
    switch ($Mode) {
        'typing' {
            if ($state.Frame % 3 -eq 0) {
                $c = $chars[$state.Rnd.Next(0, $chars.Length - 1)]
                $state.Typed += $c
                if ($state.Typed.Length -gt 90) { $state.Typed = '' }
            }
        }
        'scroll' {
            $state.ScrollY = ($state.ScrollY + 3) % 400
        }
        'noise' {
            if ($state.Frame % 2 -eq 0) {
                $w = [Math]::Min(960, $bounds.Width)
                $h = [Math]::Min(540, $bounds.Height)
                $bmp = New-Object System.Drawing.Bitmap($w, $h)
                $g2 = [System.Drawing.Graphics]::FromImage($bmp)
                $cols = 60
                $rows = 34
                $cw = [int]($w / $cols)
                $chh = [int]($h / $rows)
                for ($x = 0; $x -lt $cols; $x++) {
                    for ($yy = 0; $yy -lt $rows; $yy++) {
                        $r = $state.Rnd.Next(0, 255)
                        $gg = $state.Rnd.Next(0, 255)
                        $b = $state.Rnd.Next(0, 255)
                        $br = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb($r, $gg, $b))
                        $g2.FillRectangle($br, ($x * $cw), ($yy * $chh), ($cw + 1), ($chh + 1))
                        $br.Dispose()
                    }
                }
                $g2.Dispose()
                $old = $state.Pixels
                $state.Pixels = $bmp
                if ($null -ne $old) { $old.Dispose() }
            }
        }
    }
    $form.Invalidate()
})

$timer.Start()
$form.Show()
$form.Activate()
$deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
while ([DateTime]::UtcNow -lt $deadline) {
    [System.Windows.Forms.Application]::DoEvents()
    Start-Sleep -Milliseconds 10
}
$timer.Stop()
$form.Close()
$form.Dispose()
if ($null -ne $state.Pixels) { $state.Pixels.Dispose() }
Write-Host "testpattern 结束: display=$Display mode=$Mode seconds=$Seconds"
