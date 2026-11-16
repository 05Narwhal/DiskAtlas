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

//! Where the user is: the current folder, the path walked to it, and back/forward history.

use std::sync::Arc;

use disk_atlas_lib::fs::{FileSystem, NodeId};
use gpui::{Context, EventEmitter, SharedString};

use super::sources::{Source, SourceId};

/// The oldest history entries are dropped beyond this many.
const HISTORY_CAP: usize = 200;

/// One step of a path: a folder and the name it was reached by.
#[derive(Clone)]
pub struct Crumb {
    /// The folder.
    pub node: NodeId,
    /// Its name, for the breadcrumb.
    pub name: SharedString,
}

/// A folder the app can show: a source and the path walked from its root. `crumbs` is never empty.
#[derive(Clone)]
pub struct Location {
    source: SourceId,
    fs: Arc<dyn FileSystem>,
    crumbs: Arc<[Crumb]>,
    /// A copy of the last crumb, so `node` and `name` need no fallible lookup.
    current: Crumb,
}

impl Location {
    /// The top of a source.
    pub fn root(source: &Source) -> Self {
        let current = Crumb {
            node: source.fs.root(),
            name: source.label.clone(),
        };
        Self {
            source: source.id,
            fs: Arc::clone(&source.fs),
            crumbs: Arc::from(vec![current.clone()]),
            current,
        }
    }

    /// The source this folder belongs to.
    pub fn source(&self) -> SourceId {
        self.source
    }

    /// The source's filesystem.
    pub fn fs(&self) -> &Arc<dyn FileSystem> {
        &self.fs
    }

    /// The path from the source's root to this folder, root first.
    pub fn crumbs(&self) -> &[Crumb] {
        &self.crumbs
    }

    /// The folder itself.
    pub fn node(&self) -> NodeId {
        self.current.node
    }

    /// The folder's name.
    pub fn name(&self) -> &SharedString {
        &self.current.name
    }

    /// The folder `node`, one step below this one.
    pub fn child(&self, node: NodeId, name: impl Into<SharedString>) -> Self {
        let current = Crumb {
            node,
            name: name.into(),
        };
        let crumbs = self
            .crumbs
            .iter()
            .cloned()
            .chain(std::iter::once(current.clone()))
            .collect();
        Self {
            source: self.source,
            fs: Arc::clone(&self.fs),
            crumbs,
            current,
        }
    }

    /// The folder at the end of `crumbs`, a path from the root of this location's source. Same
    /// source and filesystem as `self`; `None` when `crumbs` is empty.
    pub fn with_crumbs(&self, crumbs: Vec<Crumb>) -> Option<Self> {
        let current = crumbs.last()?.clone();
        Some(Self {
            source: self.source,
            fs: Arc::clone(&self.fs),
            crumbs: Arc::from(crumbs),
            current,
        })
    }

    /// The folder above, or `None` at the root.
    pub fn parent(&self) -> Option<Self> {
        self.ancestor(self.crumbs.len().checked_sub(2)?)
    }

    /// The folder at `depth` on the path (0 is the root), or `None` when the path is not that deep.
    pub fn ancestor(&self, depth: usize) -> Option<Self> {
        let path = self.crumbs.get(..=depth)?;
        let current = path.last()?.clone();
        Some(Self {
            source: self.source,
            fs: Arc::clone(&self.fs),
            crumbs: Arc::from(path),
            current,
        })
    }

    /// True when both name the same folder of the same source.
    pub fn same_folder(&self, other: &Self) -> bool {
        self.source == other.source && self.current.node == other.current.node
    }
}

/// What a move produced: where we are now and which row name the list should put its cursor on.
pub struct Arrival {
    /// The folder now shown. The view reads it from `Navigation::current`; the tests read it here.
    #[cfg_attr(not(test), expect(dead_code, reason = "read by the history tests"))]
    pub location: Location,
    /// The row to put the cursor on, if the move implies one.
    pub focus: Option<SharedString>,
}

/// Back/forward history. Pure logic, no GPUI state.
#[derive(Default)]
pub struct History {
    entries: Vec<Location>,
    index: usize,
}

impl History {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// The folder shown now.
    pub fn current(&self) -> Option<&Location> {
        self.entries.get(self.index)
    }

    /// True when there is an earlier entry.
    pub fn can_go_back(&self) -> bool {
        self.index > 0
    }

    /// True when there is a later entry.
    pub fn can_go_forward(&self) -> bool {
        self.index.saturating_add(1) < self.entries.len()
    }

    /// True when the current folder has a parent.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no Up button yet; the history tests use it")
    )]
    pub fn can_go_up(&self) -> bool {
        self.current().is_some_and(|here| here.crumbs.len() > 1)
    }

    /// Goes to `to`, dropping any forward entries. `None` (and no change) when `to` is the folder
    /// already shown.
    pub fn navigate(&mut self, to: Location) -> Option<Arrival> {
        self.push(to, None)
    }

    /// Goes to the parent folder. The focus is the name of the folder just left.
    pub fn go_up(&mut self) -> Option<Arrival> {
        let here = self.current()?;
        let focus = here.name().clone();
        let parent = here.parent()?;
        self.push(parent, Some(focus))
    }

    /// Goes to the crumb at `depth`. The focus is the name of the crumb below it on the old path.
    /// `None` when `depth` is the current depth or out of range.
    pub fn go_to_crumb(&mut self, depth: usize) -> Option<Arrival> {
        let here = self.current()?;
        let below = depth.saturating_add(1);
        let focus = here.crumbs().get(below)?.name.clone();
        let target = here.ancestor(depth)?;
        self.push(target, Some(focus))
    }

    /// Steps back in history.
    pub fn go_back(&mut self) -> Option<Arrival> {
        let index = self.index.checked_sub(1)?;
        self.move_to(index)
    }

    /// Steps forward in history.
    pub fn go_forward(&mut self) -> Option<Arrival> {
        let index = self.index.checked_add(1)?;
        self.move_to(index)
    }

    fn move_to(&mut self, index: usize) -> Option<Arrival> {
        let location = self.entries.get(index)?.clone();
        self.index = index;
        Some(Arrival {
            location,
            focus: None,
        })
    }

    fn push(&mut self, to: Location, focus: Option<SharedString>) -> Option<Arrival> {
        if self.current().is_some_and(|here| here.same_folder(&to)) {
            return None;
        }
        // Keep the current entry, drop everything after it.
        self.entries.truncate(self.index.saturating_add(1));
        self.entries.push(to.clone());
        if self.entries.len() > HISTORY_CAP {
            self.entries.remove(0);
        }
        self.index = self.entries.len().saturating_sub(1);
        Some(Arrival {
            location: to,
            focus,
        })
    }
}

/// The current folder changed.
pub enum NavigationEvent {
    /// A move happened. `focus` is the row name the list should put its cursor on.
    Changed {
        /// The row to put the cursor on, if any.
        focus: Option<SharedString>,
    },
}

impl EventEmitter<NavigationEvent> for Navigation {}

/// The entity every input goes through to change the folder.
pub struct Navigation {
    history: History,
}

impl Navigation {
    /// No location yet.
    pub fn new() -> Self {
        Self {
            history: History::new(),
        }
    }

    /// The folder shown now.
    pub fn current(&self) -> Option<&Location> {
        self.history.current()
    }

    /// True when Back would do something.
    pub fn can_go_back(&self) -> bool {
        self.history.can_go_back()
    }

    /// True when Forward would do something.
    pub fn can_go_forward(&self) -> bool {
        self.history.can_go_forward()
    }

    /// Goes to `to`.
    pub fn navigate(&mut self, to: Location, cx: &mut Context<Self>) {
        let arrival = self.history.navigate(to);
        Self::arrived(arrival, cx);
    }

    /// Goes to `to` with the cursor on the row named `focus`, the way [`Self::go_up`] lands on the
    /// folder just left. Nothing happens when `to` is the folder already shown.
    pub fn navigate_focus(
        &mut self,
        to: Location,
        focus: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let arrival = self.history.push(to, focus);
        Self::arrived(arrival, cx);
    }

    /// Opens a folder inside the current one. Does nothing with no current location.
    pub fn open_child(
        &mut self,
        node: NodeId,
        name: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(to) = self.history.current().map(|here| here.child(node, name)) else {
            return;
        };
        self.navigate(to, cx);
    }

    /// Goes to the parent folder.
    pub fn go_up(&mut self, cx: &mut Context<Self>) {
        let arrival = self.history.go_up();
        Self::arrived(arrival, cx);
    }

    /// Goes to the crumb at `depth` of the current path.
    pub fn go_to_crumb(&mut self, depth: usize, cx: &mut Context<Self>) {
        let arrival = self.history.go_to_crumb(depth);
        Self::arrived(arrival, cx);
    }

    /// Steps back in history.
    pub fn go_back(&mut self, cx: &mut Context<Self>) {
        let arrival = self.history.go_back();
        Self::arrived(arrival, cx);
    }

    /// Steps forward in history.
    pub fn go_forward(&mut self, cx: &mut Context<Self>) {
        let arrival = self.history.go_forward();
        Self::arrived(arrival, cx);
    }

    fn arrived(arrival: Option<Arrival>, cx: &mut Context<Self>) {
        if let Some(arrival) = arrival {
            cx.emit(NavigationEvent::Changed {
                focus: arrival.focus,
            });
            cx.notify();
        }
    }
}

impl Default for Navigation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::sources::{Availability, SourceGroup, SourceKind};
    use disk_atlas_lib::fs::native::NativeFs;

    // `VolumeInfo` is `#[non_exhaustive]`, so a fake `FileSystem` cannot be written outside the
    // engine crate. A real `NativeFs` over the temp folder does the same job: `Location` and
    // `History` only compare node ids and never call the filesystem.
    fn source(n: u32) -> Source {
        let fs = NativeFs::new(std::env::temp_dir(), "test").expect("temp dir opens");
        Source {
            id: SourceId::for_test(n),
            group: SourceGroup::ThisComputer,
            label: SharedString::from(format!("source{n}")),
            format: None,
            fs: Arc::new(fs),
            kind: SourceKind::Host,
            availability: Availability::Browsable,
            system: None,
            role: None,
        }
    }

    /// source1 > a > b > c, with node ids 10, 11, 12 for a, b, c.
    fn deep() -> Location {
        Location::root(&source(1))
            .child(NodeId(10), "a")
            .child(NodeId(11), "b")
            .child(NodeId(12), "c")
    }

    fn names(location: &Location) -> Vec<String> {
        location
            .crumbs()
            .iter()
            .map(|crumb| crumb.name.to_string())
            .collect()
    }

    fn name_now(history: &History) -> String {
        history.current().unwrap().name().to_string()
    }

    #[test]
    fn location_root_child_parent_ancestor() {
        let src = source(1);
        let root = Location::root(&src);
        assert_eq!(root.crumbs().len(), 1);
        assert_eq!(root.node(), src.fs.root());
        assert_eq!(root.name().as_ref(), "source1");
        assert_eq!(root.source(), src.id);
        assert!(root.parent().is_none());

        let c = deep();
        assert_eq!(names(&c), ["source1", "a", "b", "c"]);
        assert_eq!(c.node(), NodeId(12));
        assert_eq!(c.name().as_ref(), "c");

        let b = c.parent().unwrap();
        assert_eq!(names(&b), ["source1", "a", "b"]);
        assert_eq!(b.node(), NodeId(11));
        assert_eq!(c.ancestor(0).unwrap().node(), src.fs.root());
        assert_eq!(c.ancestor(1).unwrap().name().as_ref(), "a");
        assert_eq!(c.ancestor(3).unwrap().node(), NodeId(12));
        assert!(c.ancestor(4).is_none());
        assert!(c.ancestor(usize::MAX).is_none());
    }

    #[test]
    fn location_same_folder() {
        let c = deep();
        assert!(c.same_folder(&c.clone()));
        assert!(c.same_folder(&c.parent().unwrap().child(NodeId(12), "renamed")));
        assert!(!c.same_folder(&c.parent().unwrap()));
        // Same node ids, different source.
        let other = Location::root(&source(2))
            .child(NodeId(10), "a")
            .child(NodeId(11), "b")
            .child(NodeId(12), "c");
        assert!(!c.same_folder(&other));
    }

    #[test]
    fn navigate_back_forward_round_trip() {
        let root = Location::root(&source(1));
        let a = root.child(NodeId(10), "a");
        let b = a.child(NodeId(11), "b");
        let mut history = History::new();
        assert!(history.current().is_none());
        assert!(!history.can_go_back() && !history.can_go_forward() && !history.can_go_up());
        assert!(history.go_back().is_none());
        assert!(history.go_forward().is_none());
        assert!(history.go_up().is_none());

        assert!(history.navigate(root).unwrap().focus.is_none());
        assert!(!history.can_go_back() && !history.can_go_up());
        history.navigate(a);
        history.navigate(b);
        assert_eq!(name_now(&history), "b");
        assert!(history.can_go_back() && !history.can_go_forward() && history.can_go_up());

        let arrival = history.go_back().unwrap();
        assert!(arrival.focus.is_none());
        assert_eq!(arrival.location.name().as_ref(), "a");
        assert!(history.can_go_forward());
        assert_eq!(history.go_back().unwrap().location.crumbs().len(), 1);
        assert!(!history.can_go_back());
        assert!(history.go_back().is_none());

        assert_eq!(history.go_forward().unwrap().location.name().as_ref(), "a");
        let arrival = history.go_forward().unwrap();
        assert!(arrival.focus.is_none());
        assert_eq!(arrival.location.name().as_ref(), "b");
        assert!(history.go_forward().is_none());
    }

    #[test]
    fn navigating_after_back_drops_forward_entries() {
        let root = Location::root(&source(1));
        let a = root.child(NodeId(10), "a");
        let b = a.child(NodeId(11), "b");
        let x = root.child(NodeId(20), "x");
        let mut history = History::new();
        history.navigate(root);
        history.navigate(a);
        history.navigate(b);
        history.go_back();
        history.go_back();
        assert!(history.can_go_forward());
        history.navigate(x);
        assert!(!history.can_go_forward());
        assert_eq!(name_now(&history), "x");
        assert_eq!(history.go_back().unwrap().location.crumbs().len(), 1);
    }

    #[test]
    fn navigating_to_the_folder_shown_changes_nothing() {
        let root = Location::root(&source(1));
        let a = root.child(NodeId(10), "a");
        let mut history = History::new();
        history.navigate(root.clone());
        history.navigate(a.clone());
        history.go_back();
        assert!(history.can_go_forward());
        assert!(history.navigate(root).is_none());
        // The forward entry survived.
        assert!(history.can_go_forward());
        assert_eq!(history.go_forward().unwrap().location.name().as_ref(), "a");
        assert!(history.navigate(a).is_none());
    }

    #[test]
    fn go_up_focuses_the_folder_left() {
        let mut history = History::new();
        history.navigate(deep());
        let arrival = history.go_up().unwrap();
        assert_eq!(arrival.focus.as_ref().map(SharedString::as_str), Some("c"));
        assert_eq!(arrival.location.name().as_ref(), "b");
        assert!(history.can_go_back());
        assert!(history.go_up().is_some());
        assert!(history.go_up().is_some());
        assert!(!history.can_go_up());
        assert!(history.go_up().is_none());
        // Up is an ordinary navigation: Back returns to the folder we came from.
        assert_eq!(history.go_back().unwrap().location.name().as_ref(), "a");
    }

    #[test]
    fn go_to_crumb_focus_and_none_cases() {
        let mut history = History::new();
        history.navigate(deep());
        // Current depth and beyond: nothing.
        assert!(history.go_to_crumb(3).is_none());
        assert!(history.go_to_crumb(4).is_none());
        assert!(history.go_to_crumb(usize::MAX).is_none());
        assert_eq!(name_now(&history), "c");

        let arrival = history.go_to_crumb(1).unwrap();
        assert_eq!(arrival.focus.as_ref().map(SharedString::as_str), Some("b"));
        assert_eq!(arrival.location.name().as_ref(), "a");
        assert!(!history.can_go_forward());
        // Back returns to the deep folder.
        assert_eq!(history.go_back().unwrap().location.name().as_ref(), "c");

        let arrival = history.go_to_crumb(0).unwrap();
        assert_eq!(arrival.focus.as_ref().map(SharedString::as_str), Some("a"));
        assert!(!history.can_go_up());
        assert!(history.go_to_crumb(0).is_none());
    }

    #[test]
    fn location_with_crumbs() {
        let here = deep();
        assert!(here.with_crumbs(Vec::new()).is_none());
        let root = here.crumbs()[0].clone();
        let path = vec![
            root.clone(),
            Crumb {
                node: NodeId(40),
                name: "x".into(),
            },
            Crumb {
                node: NodeId(41),
                name: "y".into(),
            },
        ];
        let there = here.with_crumbs(path).unwrap();
        assert_eq!(there.source(), here.source());
        assert!(Arc::ptr_eq(there.fs(), here.fs()));
        assert_eq!(names(&there), ["source1", "x", "y"]);
        assert_eq!(there.node(), NodeId(41));
        assert_eq!(there.name().as_ref(), "y");
        assert_eq!(there.parent().unwrap().node(), NodeId(40));
        // A path of the root alone is the root.
        let top = here.with_crumbs(vec![root]).unwrap();
        assert!(top.same_folder(&here.ancestor(0).unwrap()));
        assert_eq!(top.crumbs().len(), 1);
        assert!(top.parent().is_none());
    }

    /// What `Navigation::navigate_focus` does to the history.
    #[test]
    fn navigate_with_a_focus() {
        let root = Location::root(&source(1));
        let a = root.child(NodeId(10), "a");
        let mut history = History::new();
        history.navigate(root.clone());
        let arrival = history.push(a.clone(), Some("file.txt".into())).unwrap();
        assert_eq!(
            arrival.focus.as_ref().map(SharedString::as_str),
            Some("file.txt")
        );
        assert_eq!(arrival.location.node(), NodeId(10));
        assert!(history.can_go_back());
        // The folder already shown: nothing.
        assert!(history.push(a, Some("other".into())).is_none());
        assert_eq!(history.go_back().unwrap().location.crumbs().len(), 1);
    }

    #[test]
    fn history_is_capped() {
        let root = Location::root(&source(1));
        let mut history = History::new();
        let total = HISTORY_CAP + 50;
        for i in 0..total {
            let node = NodeId(u64::try_from(i).unwrap() + 100);
            history.navigate(root.child(node, format!("n{i}")));
        }
        assert_eq!(history.entries.len(), HISTORY_CAP);
        assert_eq!(name_now(&history), format!("n{}", total - 1));
        assert!(!history.can_go_forward());
        // Walk all the way back: the oldest kept entry is the first one after the cut.
        let mut steps = 0;
        while history.go_back().is_some() {
            steps += 1;
        }
        assert_eq!(steps, HISTORY_CAP - 1);
        assert_eq!(name_now(&history), format!("n{}", total - HISTORY_CAP));
    }
}
