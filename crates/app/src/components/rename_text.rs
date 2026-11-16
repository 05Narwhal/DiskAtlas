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

//! The name being edited in place, drawn where the name was: its text, the selected part on the
//! selection colour, and a caret when nothing is selected.

use gpui::{Div, div, prelude::*};

use crate::state::rename::RenameEdit;
use crate::theme::{Colors, metrics, scale::radius};

/// `edit` as a field: a plain box with the name inside. `full_width` stretches it (list rows);
/// tiles keep it to the width of the label.
pub fn rename_text(edit: &RenameEdit, colors: &Colors) -> Div {
    let text = edit.text();
    let selection = edit.selection();
    let before = &text[..selection.start];
    let selected = &text[selection.clone()];
    let after = &text[selection.end..];
    let mut field = div()
        .flex()
        .flex_row()
        .items_center()
        .max_w_full()
        .overflow_hidden()
        .px(metrics::HAIRLINE * 4.0)
        .rounded(radius::MARK)
        .bg(colors.field)
        .border(metrics::HAIRLINE)
        .border_color(colors.ink3)
        .text_color(colors.ink)
        .child(div().flex_none().child(before.to_owned()));
    if selected.is_empty() {
        field = field.child(div().flex_none().w(metrics::HAIRLINE).h_4().bg(colors.ink));
    } else {
        field = field.child(
            div()
                .flex_none()
                .bg(colors.mark)
                .text_color(colors.on_mark)
                .child(selected.to_owned()),
        );
    }
    field.child(div().flex_none().child(after.to_owned()))
}
