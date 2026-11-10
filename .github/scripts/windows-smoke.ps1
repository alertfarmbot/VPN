# Проверка сборки для Windows: запускает демо-режим, открывает чат щелчком
# мыши и сохраняет снимок экрана. Сообщения выводятся латиницей, чтобы их
# не искажала кодировка консоли.
param(
    [string]$Exe = "target\release\ferrogram.exe",
    [string]$Out = (Join-Path $env:RUNNER_TEMP "windows-demo.png")
)
$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Native {
    [StructLayout(LayoutKind.Sequential)]
    public struct POINT { public int X; public int Y; }
    [DllImport("user32.dll")]
    public static extern bool ClientToScreen(IntPtr window, ref POINT point);
    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")]
    public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
"@

function Assert-Running($process) {
    $process.Refresh()
    if ($process.HasExited) {
        throw "ferrogram exited with code $($process.ExitCode)"
    }
}

$env:FERROGRAM_HOME = Join-Path $env:RUNNER_TEMP "ferrogram-home"
$app = Start-Process -FilePath $Exe -ArgumentList "--demo" -PassThru
Start-Sleep -Seconds 6
Assert-Running $app
if ($app.MainWindowHandle -eq [IntPtr]::Zero) {
    throw "the main window did not appear"
}

# Щелчок по второй строке списка чатов; координаты заданы относительно окна.
$origin = New-Object "Native+POINT"
[Native]::ClientToScreen($app.MainWindowHandle, [ref]$origin) | Out-Null
[Native]::SetForegroundWindow($app.MainWindowHandle) | Out-Null
[Native]::SetCursorPos($origin.X + 170, $origin.Y + 157) | Out-Null
Start-Sleep -Milliseconds 300
[Native]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
[Native]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Seconds 4

$bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$bitmap.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "screenshot saved to $Out"

Assert-Running $app
Write-Host ("exe size: {0:N1} MB" -f ((Get-Item $Exe).Length / 1MB))
Write-Host ("working set: {0:N1} MB, private memory: {1:N1} MB" -f ($app.WorkingSet64 / 1MB), ($app.PrivateMemorySize64 / 1MB))
Stop-Process -Id $app.Id
