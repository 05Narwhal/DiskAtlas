// Disk Atlas - A cross-platform filesystem explorer.
//
// Copyright (C) 2026 Julian A. Loy.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
//
// See the LICENSE file for the full license text.

//! Keeping a window above the others. GPUI 0.2.2 has no call for it, so this goes to the
//! operating system by the window's native handle.
//!
//! - Windows: `SetWindowPos` with `HWND_TOPMOST` or `HWND_NOTOPMOST`.
//! - macOS and Linux: not done; the setting is hidden there (macOS
//!   needs `objc` for `setLevel`, X11 has no call GPUI exposes and Wayland forbids it).

use gpui::Window;

/// Puts `window` above all other windows (`on`), or back among them. A no-op off Windows (the setting is
/// hidden there); a failure is logged, never fatal.
pub fn set(window: &Window, on: bool) {
    #[cfg(windows)]
    windows::set(window, on);
    #[cfg(not(windows))]
    let _ = (window, on);
}

#[cfg(windows)]
mod windows {
    use gpui::Window;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };

    #[allow(unsafe_code)]
    pub(super) fn set(window: &Window, on: bool) {
        let Ok(handle) = HasWindowHandle::window_handle(window) else {
            return;
        };
        let RawWindowHandle::Win32(win32) = handle.as_raw() else {
            return;
        };
        let hwnd: HWND = win32.hwnd.get() as HWND;
        let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        // SAFETY: `hwnd` is the live top-level window GPUI owns for `window`, obtained from its
        // window handle during this call on the UI thread that owns it. `after` is one of the
        // two documented sentinel values. SWP_NOMOVE and SWP_NOSIZE make the position and size
        // arguments ignored, so the zeros are unused, and SWP_NOACTIVATE leaves focus alone. The
        // call keeps no pointer after it returns.
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        if ok == 0 {
            tracing::warn!("SetWindowPos failed; the window keeps its stacking");
        }
    }
}
