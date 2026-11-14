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

//! Whether the operating system asks apps for less motion (`r1-redesign.md` 2.3).
//!
//! - Windows: "Show animations in Windows", `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`.
//! - Linux: GNOME's `enable-animations`, read with the `gsettings` command under a timeout.
//! - macOS: not asked yet (see [`system_reduces_motion`]); the setting and the environment
//!   variable still work.
//!
//! [`system_reduces_motion`] blocks (a command on Linux) and runs on the background executor.

#[cfg(any(target_os = "linux", test))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "linux", test))]
use std::time::{Duration, Instant};

/// How long `gsettings` may take before the answer counts as unknown.
#[cfg(target_os = "linux")]
const COMMAND_TIMEOUT: Duration = Duration::from_millis(500);

/// How often a running command is checked for having exited.
#[cfg(any(target_os = "linux", test))]
const COMMAND_POLL: Duration = Duration::from_millis(10);

/// `Some(true)` when the system asks for less motion, `Some(false)` when it allows animation,
/// `None` when it could not be asked. Blocking; call it off the UI thread.
pub fn system_reduces_motion() -> Option<bool> {
    #[cfg(windows)]
    {
        windows::client_area_animation().map(|enabled| !enabled)
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("gsettings");
        command.args(["get", "org.gnome.desktop.interface", "enable-animations"]);
        run_with_timeout(command, COMMAND_TIMEOUT)
            .as_deref()
            .and_then(parse_enable_animations)
    }
    #[cfg(target_os = "macos")]
    {
        // TODO(decision 20): ask `NSWorkspace.sharedWorkspace.accessibilityDisplayShouldReduceMotion`
        // through `objc` (approved as a direct dependency, not added yet) once a Mac is at hand to
        // test it. Until then macOS relies on the Motion setting and DISK_ATLAS_REDUCED_MOTION.
        None
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Reads the output of `gsettings get org.gnome.desktop.interface enable-animations`: `true`
/// (animations on) means motion is not reduced. Anything else is unknown.
#[cfg(any(target_os = "linux", test))]
fn parse_enable_animations(output: &str) -> Option<bool> {
    match output.trim() {
        "true" => Some(false),
        "false" => Some(true),
        _ => None,
    }
}

/// Runs `command` and returns its standard output, or `None` when it cannot start, fails, or is
/// still running after `timeout` (it is then killed).
#[cfg(any(target_os = "linux", test))]
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<String> {
    use std::io::Read;

    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(COMMAND_POLL),
            Ok(None) | Err(_) => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    // The answer is one short line; it is already in the pipe.
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    use windows_sys::core::BOOL;

    /// "Show animations in Windows": `Some(true)` when on, `None` when the call fails.
    #[allow(unsafe_code)]
    pub(super) fn client_area_animation() -> Option<bool> {
        let mut enabled: BOOL = 0;
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through `pvParam` and reads nothing
        // else; `enabled` is a live, aligned, writable BOOL on this stack frame for the whole
        // call, `uiParam` must be 0 for this action, and 0 for `fWinIni` asks for no broadcast
        // and no profile update. The call keeps no pointer after it returns.
        let ok = unsafe {
            SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, (&raw mut enabled).cast(), 0)
        };
        (ok != 0).then_some(enabled != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gsettings_answers() {
        assert_eq!(parse_enable_animations("true\n"), Some(false));
        assert_eq!(parse_enable_animations("false\n"), Some(true));
        assert_eq!(parse_enable_animations("  false  "), Some(true));
        assert_eq!(parse_enable_animations(""), None);
        assert_eq!(parse_enable_animations("No such key"), None);
        assert_eq!(parse_enable_animations("'true'"), None);
    }

    fn shell(script: &str) -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", script]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
    }

    #[test]
    fn a_command_that_answers_is_read() {
        let output = run_with_timeout(shell("echo true"), Duration::from_secs(10));
        assert_eq!(
            output.as_deref().and_then(parse_enable_animations),
            Some(false)
        );
    }

    #[test]
    fn a_failing_or_missing_command_is_unknown() {
        assert_eq!(
            run_with_timeout(shell("exit 3"), Duration::from_secs(10)),
            None
        );
        let missing = Command::new("disk-atlas-no-such-command-for-a-test");
        assert_eq!(run_with_timeout(missing, Duration::from_secs(10)), None);
    }

    #[test]
    fn a_command_that_hangs_is_cut_off() {
        let slow = if cfg!(windows) {
            shell("ping -n 30 127.0.0.1 >NUL")
        } else {
            shell("sleep 30")
        };
        let started = Instant::now();
        assert_eq!(run_with_timeout(slow, Duration::from_millis(100)), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(windows)]
    #[test]
    fn windows_answers() {
        // Either answer is fine; the call itself must succeed.
        assert!(windows::client_area_animation().is_some());
    }
}
