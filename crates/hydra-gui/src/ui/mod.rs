// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod categories;
pub mod menu;
pub mod probe;
pub mod table;
pub mod toolbar;

use crate::app::{El, Message};
use iced::widget::{container, scrollable, Scrollable};

/// The room a vertical scrollbar needs beside the content: iced's default
/// bar is 10px wide, plus a little air.
const GUTTER: f32 = 14.0;

/// A vertical scrollable whose content keeps clear of the scrollbar.
///
/// iced floats the bar over the content instead of beside it, so the tail
/// of any line that reaches the right edge — which a translated line does
/// far more often than an English one — is drawn underneath it. The gutter
/// goes inside the scrollable on purpose: padding the parent moves the bar
/// out with the content and changes nothing.
pub fn scroll<'a>(content: impl Into<El<'a>>) -> Scrollable<'a, Message> {
    scrollable(container(content).padding(iced::padding::right(GUTTER)))
}
