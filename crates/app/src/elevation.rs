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

//! Restarting with administrator rights, for reading physical disks.
//!
//! Windows relaunches the app through the UAC prompt. Linux and macOS have no
//! button until the read-only helper process exists; the interface shows
//! [`manual_hint`] instead.

/// What became of a restart request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// The elevated copy is starting. The caller quits this one.
    Started,
    /// The user declined the prompt. Nothing changed.
    Declined,
}

/// True where the app can relaunch itself with administrator rights.
pub fn can_restart_elevated() -> bool {
    #[cfg(windows)]
    {
        win::can_restart_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Starts a new copy of the app with administrator rights. Blocks until the
/// system prompt is answered, so call it from the background executor.
pub fn restart_elevated() -> anyhow::Result<Restart> {
    #[cfg(windows)]
    {
        win::restart_elevated()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("restarting with administrator rights is not supported on this platform")
    }
}

/// What to tell the user where there is no button.
pub fn manual_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Start Disk Atlas from Terminal with sudo to read physical disks."
    } else {
        "Start Disk Atlas with sudo or pkexec to read physical disks."
    }
}

#[cfg(any(windows, test))]
const QUOTE: u16 = b'"' as u16;
#[cfg(any(windows, test))]
const BACKSLASH: u16 = b'\\' as u16;

/// Joins UTF-16 arguments into one command-line tail that `CommandLineToArgvW`
/// and the Microsoft C runtime split back into exactly the same arguments.
///
/// Only for the arguments after the program name: `argv[0]` follows different
/// rules, and `ShellExecuteExW` takes the program separately anyway.
#[cfg(any(windows, test))]
fn join_command_line<A: AsRef<[u16]>>(args: impl IntoIterator<Item = A>) -> Vec<u16> {
    let mut out = Vec::new();
    for (i, arg) in args.into_iter().enumerate() {
        if i > 0 {
            out.push(u16::from(b' '));
        }
        append_quoted(arg.as_ref(), &mut out);
    }
    out
}

/// Appends one argument, quoted only when it must be.
///
/// Inside quotes, a run of backslashes is literal unless a quote follows it.
/// Before a quote (an escaped one inside the argument, or the closing one)
/// the run is doubled, and an escaped quote gets one more backslash.
#[cfg(any(windows, test))]
fn append_quoted(arg: &[u16], out: &mut Vec<u16>) {
    // Space, tab, newline, vertical tab and quote split or end an argument.
    let needs_quotes = arg.is_empty()
        || arg
            .iter()
            .any(|&c| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | QUOTE));
    if !needs_quotes {
        out.extend_from_slice(arg);
        return;
    }

    out.push(QUOTE);
    let mut backslashes = 0usize;
    for &c in arg {
        match c {
            BACKSLASH => backslashes = backslashes.saturating_add(1),
            QUOTE => {
                let run = backslashes.saturating_mul(2).saturating_add(1);
                out.extend(std::iter::repeat_n(BACKSLASH, run));
                out.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // The closing quote follows, so trailing backslashes are doubled.
    out.extend(std::iter::repeat_n(
        BACKSLASH,
        backslashes.saturating_mul(2),
    ));
    out.push(QUOTE);
}

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows_sys::Win32::Foundation::{ERROR_CANCELLED, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    use super::{Restart, join_command_line};

    /// `runas`, NUL-terminated UTF-16: the verb that asks UAC for elevation.
    /// A `static`, not a `const`, so the pointer handed to the shell refers to
    /// one place that lives for the whole program rather than a temporary.
    static RUNAS: [u16; 6] = [
        b'r' as u16,
        b'u' as u16,
        b'n' as u16,
        b'a' as u16,
        b's' as u16,
        0,
    ];

    pub(super) fn can_restart_elevated() -> bool {
        match process_is_elevated() {
            Ok(elevated) => !elevated,
            Err(err) => {
                // Offering a restart that turns out unnecessary is harmless;
                // hiding the only way to read disks is not.
                tracing::warn!(%err, "could not read the process token's elevation");
                true
            }
        }
    }

    pub(super) fn restart_elevated() -> anyhow::Result<Restart> {
        let exe = std::env::current_exe()?;
        let args: Vec<Vec<u16>> = std::env::args_os()
            .skip(1)
            .map(|a| a.encode_wide().collect())
            .collect();
        let params = join_command_line(&args);
        match shell_execute_runas(exe.as_os_str(), &params) {
            Ok(()) => Ok(Restart::Started),
            Err(err) if err.raw_os_error() == Some(ERROR_CANCELLED as i32) => Ok(Restart::Declined),
            Err(err) => {
                Err(anyhow::Error::new(err).context("could not restart with administrator rights"))
            }
        }
    }

    /// True when this process runs with an elevated (full administrator) token.
    #[allow(unsafe_code)]
    fn process_is_elevated() -> io::Result<bool> {
        let mut raw: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess returns a pseudo-handle that is always
        // valid in this process and needs no closing. `raw` is a live local
        // HANDLE the call writes the new token handle into.
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcessToken succeeded, so `raw` is an open token handle
        // that nothing else owns or closes. OwnedHandle takes sole ownership
        // and closes it exactly once, on every return path.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };

        let mut elevation = TOKEN_ELEVATION::default();
        const SIZE: u32 = size_of::<TOKEN_ELEVATION>() as u32;
        let mut returned = 0u32;
        // SAFETY: `token` is open with TOKEN_QUERY access and borrowed for the
        // whole call. The buffer pointer and SIZE describe the live, writable
        // `elevation` local, which is exactly the TOKEN_ELEVATION that the
        // TokenElevation class writes; `returned` is a live u32.
        let ok = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenElevation,
                (&raw mut elevation).cast(),
                SIZE,
                &mut returned,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(elevation.TokenIsElevated != 0)
    }

    /// Runs `file` with `params` through the `runas` verb. Blocks while the
    /// UAC prompt is up.
    #[allow(unsafe_code)]
    fn shell_execute_runas(file: &OsStr, params: &[u16]) -> io::Result<()> {
        let file = to_wide_nul(file.encode_wide())?;
        let params = to_wide_nul(params.iter().copied())?;

        let info_size = u32::try_from(size_of::<SHELLEXECUTEINFOW>())
            .map_err(|_| io::Error::other("SHELLEXECUTEINFOW size does not fit in u32"))?;
        let mut info = SHELLEXECUTEINFOW {
            cbSize: info_size,
            // NOASYNC: the launch is complete when the call returns, because
            // the caller quits this process right after. FLAG_NO_UI: errors
            // come back to the caller instead of a shell message box.
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpVerb: RUNAS.as_ptr(),
            lpFile: file.as_ptr(),
            lpParameters: params.as_ptr(),
            nShow: SW_SHOWNORMAL,
            // Zeroed: no owner window, no directory, no class, no ID list.
            ..SHELLEXECUTEINFOW::default()
        };
        // SAFETY: `info` is a live, exclusively borrowed SHELLEXECUTEINFOW,
        // zero-initialised with cbSize set to its size. lpVerb, lpFile and
        // lpParameters point to NUL-terminated UTF-16 buffers (RUNAS is a
        // `static`; `file` and `params` are locals not moved or dropped
        // before this call returns) that outlive the call, and
        // every other pointer field is null. Without SEE_MASK_NOCLOSEPROCESS
        // no process handle is returned, so nothing needs closing.
        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Copies UTF-16 units into a NUL-terminated buffer, refusing interior
    /// NULs that would silently cut the string short.
    fn to_wide_nul(units: impl Iterator<Item = u16>) -> io::Result<Vec<u16>> {
        let mut out: Vec<u16> = units.collect();
        if out.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path or argument contains a NUL character",
            ));
        }
        out.push(0);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn join(args: &[&str]) -> String {
        let line = join_command_line(args.iter().map(|a| wide(a)));
        String::from_utf16(&line).expect("test input is valid UTF-16")
    }

    /// Splits a command-line tail the way the Microsoft C runtime does, for
    /// the inputs `join_command_line` produces (it never emits `""` inside a
    /// quoted argument, so that rule is not modelled).
    fn split(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let mut args = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            while i < chars.len() && matches!(chars[i], ' ' | '\t') {
                i += 1;
            }
            if i == chars.len() {
                break;
            }
            let mut arg = String::new();
            let mut quoted = false;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' {
                    let start = i;
                    while i < chars.len() && chars[i] == '\\' {
                        i += 1;
                    }
                    let run = i - start;
                    if i < chars.len() && chars[i] == '"' {
                        arg.extend(std::iter::repeat_n('\\', run / 2));
                        if run % 2 == 1 {
                            arg.push('"');
                            i += 1;
                        }
                    } else {
                        arg.extend(std::iter::repeat_n('\\', run));
                    }
                    continue;
                }
                if c == '"' {
                    quoted = !quoted;
                } else if !quoted && matches!(c, ' ' | '\t') {
                    break;
                } else {
                    arg.push(c);
                }
                i += 1;
            }
            args.push(arg);
        }
        args
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(join(&[]), "");
        assert_eq!(join(&["a", "b"]), "a b");
        assert_eq!(join(&[r"C:\dir\"]), r"C:\dir\");
        assert_eq!(join(&[r"a\\b"]), r"a\\b");
    }

    #[test]
    fn spaces_and_whitespace_are_quoted() {
        assert_eq!(join(&["hello world"]), r#""hello world""#);
        assert_eq!(join(&["a\tb"]), "\"a\tb\"");
        assert_eq!(join(&["a\nb"]), "\"a\nb\"");
    }

    #[test]
    fn empty_arguments_survive() {
        assert_eq!(join(&[""]), r#""""#);
        assert_eq!(join(&["a", "", "b"]), r#"a "" b"#);
    }

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(join(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(join(&[r#"a"b"#]), r#""a\"b""#);
        assert_eq!(join(&[r#"""#]), r#""\"""#);
    }

    #[test]
    fn backslashes_before_quotes_are_doubled() {
        // One backslash before a quote: two for the backslash, one for the quote.
        assert_eq!(join(&[r#"a\"b"#]), r#""a\\\"b""#);
        // Trailing backslashes before the closing quote.
        assert_eq!(join(&[r"C:\Program Files\"]), r#""C:\Program Files\\""#);
        assert_eq!(join(&[r"a b\\"]), r#""a b\\\\""#);
        // Backslashes not before a quote stay as they are.
        assert_eq!(join(&[r"a\\b c"]), r#""a\\b c""#);
    }

    #[test]
    fn joined_arguments_split_back_unchanged() {
        let cases: &[&[&str]] = &[
            &["plain", "two words", ""],
            &[r"C:\Program Files\", r#"say "hi""#, r#"a\"b"#],
            &[r#"\\""#, r"\\", r#"x\\"y"#, "\t", r#"""#],
            &["näme", "日本 語", r"\\server\share\dir\"],
        ];
        for args in cases {
            let line = join(args);
            assert_eq!(split(&line), *args, "line: {line}");
        }
    }

    #[test]
    fn can_restart_elevated_does_not_panic() {
        let can = can_restart_elevated();
        if cfg!(not(windows)) {
            assert!(!can);
        }
    }
}
