# screenshot tools

`shot.ps1` drives the running Disk Atlas window so UI work can be checked by
eye. Windows only, no dependencies. The window title is `Disk Atlas`; `-Title`
is a case-insensitive substring match against visible top-level windows (the
largest match wins). Every subcommand exits 2 with a message on stderr when the
window or executable is not found.

```
pwsh tools/screenshot/shot.ps1 <subcommand> ...      (powershell.exe works too)
```

| Subcommand | Usage |
|---|---|
| `launch` | `launch -Exe target\debug\disk-atlas.exe -Title "Disk Atlas" [-WaitSeconds 20]` starts the app, waits for its window, prints `pid=N`. |
| `capture` | `capture -Title "Disk Atlas" -Out shot.png` brings the window forward, grabs its visible rectangle with CopyFromScreen (PrintWindow `PW_RENDERFULLCONTENT` is the fallback), saves a PNG, prints its full path. |
| `click` | `click -Title "Disk Atlas" -X 300 -Y 200 [-Right] [-Double]` clicks at pixels relative to the window's top-left (same origin as `capture` images). |
| `type` | `type -Title "Disk Atlas" -Text "hello"` sends Unicode characters. |
| `key` | `key -Title "Disk Atlas" -Keys ctrl+f` sends one key or chord. Names: Space, Escape, Enter, Tab, Backspace, Delete, Home, End, PageUp, PageDown, Left, Right, Up, Down, A-Z, 0-9, F1-F12. Modifiers: ctrl, shift, alt, win. |

Typical loop: `launch`, `capture`, `click`/`key`, `capture`, then close the app
(`Stop-Process -Name disk-atlas`).

## Notes and limits

- Input uses SendInput, so the window must be in the foreground and the
  screen unlocked. The tool takes the foreground first; do not touch the mouse
  meanwhile. Clicks move the real cursor.
- Capture is a screen grab, so anything overlapping the window (other windows,
  the cursor-adjacent tooltips) appears in it. Light and dark are switched in
  the app or by the OS setting, not by this tool.
- The process is made per-monitor DPI aware, so coordinates are physical pixels.
- Windows spanning monitors with different scaling may be offset.
