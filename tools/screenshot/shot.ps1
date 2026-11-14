# Disk Atlas UI-verification helper: launch, capture, click, type, key.
# Usage: see README.md. Windows only.
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet('capture', 'click', 'type', 'key', 'launch')]
    [string]$Command,
    [string]$Title,
    [string]$Out,
    [int]$X,
    [int]$Y,
    [switch]$Right,
    [switch]$Double,
    [string]$Text,
    [string]$Keys,
    [string]$Exe,
    [int]$WaitSeconds = 20
)
$ErrorActionPreference = 'Stop'

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class ShotNative {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] i, int size);
    [DllImport("user32.dll")] static extern bool SetProcessDpiAwarenessContext(IntPtr c);
    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte sc, uint flags, UIntPtr extra);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int attr, out RECT r, int size);

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx, dy; public uint data, flags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort vk, scan; public uint flags, time; public IntPtr extra; }
    [StructLayout(LayoutKind.Explicit)] public struct IU {
        [FieldOffset(0)] public MOUSEINPUT mi;
        [FieldOffset(0)] public KEYBDINPUT ki;
    }
    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public IU u; }

    public static void MakeDpiAware() {
        try { if (SetProcessDpiAwarenessContext(new IntPtr(-4))) return; } catch { }
        try { SetProcessDPIAware(); } catch { }
    }

    // Visible top-level window whose title contains `sub`; the largest wins.
    public static IntPtr Find(string sub) {
        IntPtr best = IntPtr.Zero; long bestArea = -1;
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            var sb = new StringBuilder(512);
            GetWindowText(h, sb, 512);
            if (sb.Length == 0 || sb.ToString().IndexOf(sub, StringComparison.OrdinalIgnoreCase) < 0) return true;
            RECT r; GetWindowRect(h, out r);
            long a = (long)(r.R - r.L) * (r.B - r.T);
            if (a > bestArea) { bestArea = a; best = h; }
            return true;
        }, IntPtr.Zero);
        return best;
    }

    // Visible frame rectangle (excludes the invisible resize border on Windows 10/11).
    public static RECT Bounds(IntPtr h) {
        RECT r;
        if (DwmGetWindowAttribute(h, 9, out r, Marshal.SizeOf(typeof(RECT))) != 0) GetWindowRect(h, out r);
        return r;
    }

    public static void Foreground(IntPtr h) {
        if (IsIconic(h)) ShowWindow(h, 9);
        keybd_event(0x12, 0, 0, UIntPtr.Zero);        // Alt tap lifts the foreground lock
        keybd_event(0x12, 0, 2, UIntPtr.Zero);
        SetForegroundWindow(h);
    }

    public static bool Print(IntPtr h, IntPtr hdc) { return PrintWindow(h, hdc, 2); }

    static void Send(INPUT[] a) { SendInput((uint)a.Length, a, Marshal.SizeOf(typeof(INPUT))); }

    public static void Click(int x, int y, bool right, bool dbl) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(60);
        uint down = right ? 0x8u : 0x2u, up = right ? 0x10u : 0x4u;
        for (int n = 0; n < (dbl ? 2 : 1); n++) {
            var i = new INPUT[2];
            i[0].type = 0; i[0].u.mi.flags = down;
            i[1].type = 0; i[1].u.mi.flags = up;
            Send(i);
            System.Threading.Thread.Sleep(60);
        }
    }

    static INPUT Key(ushort vk, ushort scan, uint flags) {
        var i = new INPUT(); i.type = 1; i.u.ki.vk = vk; i.u.ki.scan = scan; i.u.ki.flags = flags; return i;
    }

    public static void TypeText(string s) {
        foreach (char c in s) {
            Send(new[] { Key(0, c, 4), Key(0, c, 4 | 2) });   // KEYEVENTF_UNICODE, then | KEYUP
            System.Threading.Thread.Sleep(15);
        }
    }

    const uint EXT = 1;
    public static void Chord(ushort[] mods, ushort vk, bool extended) {
        var l = new List<INPUT>();
        foreach (var m in mods) l.Add(Key(m, 0, 0));
        l.Add(Key(vk, 0, extended ? EXT : 0));
        l.Add(Key(vk, 0, (extended ? EXT : 0) | 2));
        for (int i = mods.Length - 1; i >= 0; i--) l.Add(Key(mods[i], 0, 2));
        Send(l.ToArray());
    }
}
'@

Add-Type -AssemblyName System.Drawing
[ShotNative]::MakeDpiAware()

function Fail([string]$msg) {
    [Console]::Error.WriteLine("shot.ps1: $msg")
    exit 2
}

function Get-Win([string]$t) {
    if (-not $t) { Fail 'missing -Title' }
    $h = [ShotNative]::Find($t)
    if ($h -eq [IntPtr]::Zero) { Fail "no visible window with title containing '$t'" }
    return $h
}

function Focus-Win($h) {
    [ShotNative]::Foreground($h)
    Start-Sleep -Milliseconds 300
}

$ModVk = @{ ctrl = 0x11; control = 0x11; shift = 0x10; alt = 0x12; win = 0x5B }
$KeyVk = @{
    space = 0x20; escape = 0x1B; esc = 0x1B; enter = 0x0D; return = 0x0D; tab = 0x09
    backspace = 0x08; delete = 0x2E; home = 0x24; end = 0x23; pageup = 0x21; pagedown = 0x22
    left = 0x25; up = 0x26; right = 0x27; down = 0x28
}
$ExtendedKeys = 'delete', 'home', 'end', 'pageup', 'pagedown', 'left', 'up', 'right', 'down'

switch ($Command) {
    'launch' {
        if (-not $Exe -or -not (Test-Path -LiteralPath $Exe)) { Fail "executable not found: '$Exe'" }
        if (-not $Title) { Fail 'missing -Title' }
        $p = Start-Process -FilePath $Exe -PassThru
        $deadline = (Get-Date).AddSeconds($WaitSeconds)
        while ((Get-Date) -lt $deadline) {
            if ([ShotNative]::Find($Title) -ne [IntPtr]::Zero) {
                Start-Sleep -Milliseconds 800   # let the first frame render
                Write-Output "pid=$($p.Id)"
                exit 0
            }
            if ($p.HasExited) { Fail "process exited with code $($p.ExitCode) before a window appeared" }
            Start-Sleep -Milliseconds 200
        }
        Fail "window '$Title' did not appear within $WaitSeconds s (pid $($p.Id) still running)"
    }
    'capture' {
        if (-not $Out) { Fail 'missing -Out' }
        $h = Get-Win $Title
        Focus-Win $h
        $r = [ShotNative]::Bounds($h)
        $w = $r.R - $r.L; $ht = $r.B - $r.T
        if ($w -le 0 -or $ht -le 0) { Fail 'window has an empty rectangle (minimised?)' }
        $bmp = New-Object System.Drawing.Bitmap $w, $ht
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        try {
            $g.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size $w, $ht))
        } catch {
            # Fallback: PrintWindow with PW_RENDERFULLCONTENT.
            $hdc = $g.GetHdc()
            try { [void][ShotNative]::Print($h, $hdc) } finally { $g.ReleaseHdc($hdc) }
        }
        $g.Dispose()
        $dir = Split-Path -Parent ([IO.Path]::GetFullPath($Out))
        if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
        $full = [IO.Path]::GetFullPath($Out)
        $bmp.Save($full, [System.Drawing.Imaging.ImageFormat]::Png)
        $bmp.Dispose()
        Write-Output $full
    }
    'click' {
        $h = Get-Win $Title
        Focus-Win $h
        $r = [ShotNative]::Bounds($h)
        [ShotNative]::Click($r.L + $X, $r.T + $Y, [bool]$Right, [bool]$Double)
        Start-Sleep -Milliseconds 200
    }
    'type' {
        $h = Get-Win $Title
        Focus-Win $h
        [ShotNative]::TypeText($Text)
    }
    'key' {
        if (-not $Keys) { Fail 'missing -Keys' }
        $h = Get-Win $Title
        Focus-Win $h
        $parts = $Keys.ToLowerInvariant().Split('+')
        $mods = New-Object System.Collections.Generic.List[uint16]
        for ($i = 0; $i -lt $parts.Count - 1; $i++) {
            $m = $parts[$i]
            if (-not $ModVk.ContainsKey($m)) { Fail "unknown modifier '$m'" }
            $mods.Add([uint16]$ModVk[$m])
        }
        $k = $parts[-1]
        if ($KeyVk.ContainsKey($k)) { $vk = [uint16]$KeyVk[$k] }
        elseif ($k -match '^[a-z]$') { $vk = [uint16][int][char]$k.ToUpperInvariant() }
        elseif ($k -match '^[0-9]$') { $vk = [uint16][int][char]$k }
        elseif ($k -match '^f([1-9]|1[0-2])$') { $vk = [uint16](0x6F + [int]$Matches[1]) }
        else { Fail "unknown key '$k'" }
        [ShotNative]::Chord($mods.ToArray(), $vk, ($ExtendedKeys -contains $k))
        Start-Sleep -Milliseconds 150
    }
}
