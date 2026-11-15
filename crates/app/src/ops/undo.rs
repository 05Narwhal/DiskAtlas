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

//! Undo for host file operations (`r1-redesign.md` 4): session only, at most [`MAX_STEPS`].

use std::path::PathBuf;

use super::HostPath;

/// Most steps kept.
pub const MAX_STEPS: usize = 50;

/// One thing that was done, with what is needed to take it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `from` was renamed or moved to `to`.
    Moved { from: PathBuf, to: PathBuf },
    /// An empty folder was made.
    NewFolder { path: PathBuf },
    /// Items were copied; the copies are `created`.
    Copied { created: Vec<PathBuf> },
    /// Items that were at these paths went to the Recycle Bin.
    Trashed { originals: Vec<PathBuf> },
}

impl Step {
    /// What undoing it is called, for the menu and the failure sentence.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Moved { .. } => "Undo move or rename",
            Self::NewFolder { .. } => "Undo new folder",
            Self::Copied { .. } => "Undo copy",
            Self::Trashed { .. } => "Undo move to the Recycle Bin",
        }
    }

    /// Takes the step back. Blocking. The error is a sentence for the user.
    pub fn undo(&self) -> Result<(), String> {
        match self {
            Self::Moved { from, to } => {
                if from.symlink_metadata().is_ok() {
                    return Err(format!(
                        "\u{201c}{}\u{201d} is back in its place already.",
                        name_of(from)
                    ));
                }
                std::fs::rename(to, from)
                    .map_err(|error| format!("Could not move \u{201c}{}\u{201d} back: {error}", name_of(to)))
            }
            Self::NewFolder { path } => std::fs::remove_dir(path).map_err(|error| {
                format!("Could not remove \u{201c}{}\u{201d}: {error}", name_of(path))
            }),
            Self::Copied { created } => {
                let paths: Vec<HostPath> = created.iter().cloned().map(HostPath::new_unchecked).collect();
                super::trash(&paths)
            }
            Self::Trashed { originals } => restore(originals),
        }
    }
}

fn name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn restore(originals: &[PathBuf]) -> Result<(), String> {
    let items = trash::os_limited::list().map_err(|error| error.to_string())?;
    // The newest bin entry per original path: what was just deleted.
    let mut chosen = Vec::new();
    for original in originals {
        let found = items
            .iter()
            .filter(|item| item.original_path() == *original)
            .max_by_key(|item| item.time_deleted);
        match found {
            Some(item) => chosen.push(item.clone()),
            None => {
                return Err(format!(
                    "\u{201c}{}\u{201d} is no longer in the Recycle Bin.",
                    name_of(original)
                ));
            }
        }
    }
    trash::os_limited::restore_all(chosen).map_err(|error| error.to_string())
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
fn restore(_originals: &[PathBuf]) -> Result<(), String> {
    Err("Restore it from the Trash in Finder.".to_owned())
}

/// The steps that can be undone, newest last.
#[derive(Debug, Default)]
pub struct UndoStack {
    steps: Vec<Step>,
}

impl UndoStack {
    /// Records a step, forgetting the oldest beyond [`MAX_STEPS`].
    pub fn push(&mut self, step: Step) {
        self.steps.push(step);
        if self.steps.len() > MAX_STEPS {
            self.steps.remove(0);
        }
    }

    /// The step Ctrl+Z would take back.
    pub fn last(&self) -> Option<&Step> {
        self.steps.last()
    }

    /// Takes the newest step off the stack.
    pub fn pop(&mut self) -> Option<Step> {
        self.steps.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::tests::Temp;

    #[test]
    fn the_stack_keeps_the_newest_fifty() {
        let mut stack = UndoStack::default();
        for n in 0..60 {
            stack.push(Step::NewFolder {
                path: PathBuf::from(n.to_string()),
            });
        }
        assert_eq!(stack.steps.len(), MAX_STEPS);
        assert_eq!(
            stack.pop(),
            Some(Step::NewFolder {
                path: PathBuf::from("59")
            })
        );
        assert_eq!(
            stack.steps[0],
            Step::NewFolder {
                path: PathBuf::from("10")
            }
        );
    }

    #[test]
    fn undoing_a_move_puts_it_back_unless_the_place_is_taken() {
        let dir = Temp::new("undo");
        let (from, to) = (dir.0.join("a.txt"), dir.0.join("b.txt"));
        std::fs::write(&to, b"x").unwrap();
        let step = Step::Moved {
            from: from.clone(),
            to: to.clone(),
        };
        std::fs::write(&from, b"other").unwrap();
        assert!(step.undo().is_err());
        assert!(to.exists());
        std::fs::remove_file(&from).unwrap();
        step.undo().unwrap();
        assert!(from.exists() && !to.exists());
    }

    #[test]
    fn a_new_folder_is_only_undone_while_empty() {
        let dir = Temp::new("undofolder");
        let path = dir.0.join("New folder");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("kept.txt"), b"x").unwrap();
        let step = Step::NewFolder { path: path.clone() };
        assert!(step.undo().is_err());
        std::fs::remove_file(path.join("kept.txt")).unwrap();
        step.undo().unwrap();
        assert!(!path.exists());
    }
}
