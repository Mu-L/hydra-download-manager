// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Manage columns: which columns the download table shows, and in what
//! order. Reachable from View > Columns and from the header's own menu.
//!
//! The rows are the table's columns read top to bottom as the header reads
//! them left to right, so the arrows on each row move that column one place
//! towards either end. Edits apply to the table as they are made — the
//! dialog is a view onto the setting, not a form to submit.

use crate::app::{App, El, Message, WinKind};
use crate::model::Column;
use crate::windows::{dlg_btn_auto, dlg_btn_primary};
use crate::{i18n::tr, theme};
use iced::widget::{button, checkbox, column, container, row, scrollable, text};
use iced::Length;

/// One of the two move arrows. Disabled at the end it cannot move past.
fn arrow<'a>(glyph: &'a str, col: Column, left: bool, enabled: bool) -> El<'a> {
    let mut b = button(text(glyph).size(theme::FONT_SIZE))
        .padding([2, 8])
        .style(theme::btn);
    if enabled {
        b = b.on_press(Message::ColMove(col, left));
    }
    b.into()
}

pub fn view(app: &App) -> El<'_> {
    let cols = &app.cfg.settings.columns;
    let mut list = column![].spacing(6);
    for (i, pref) in cols.iter().enumerate() {
        // File Name identifies the row, so it is the one column that cannot
        // be hidden; its checkbox is shown ticked and inert rather than
        // hidden, so the list still reads as the whole table.
        let toggle = checkbox(pref.visible)
            .label(tr(pref.id.label()))
            .size(15.0)
            .text_size(theme::FONT_SIZE)
            .style(theme::check);
        let toggle = if pref.id == Column::Name {
            toggle
        } else {
            toggle.on_toggle(move |_| Message::ColToggle(pref.id))
        };
        list = list.push(
            row![
                container(toggle).width(Length::Fill),
                arrow("\u{25b2}", pref.id, true, i > 0),
                arrow("\u{25bc}", pref.id, false, i + 1 < cols.len()),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
    }

    container(
        column![
            text(tr("The first column is the leftmost one in the list."))
                .size(theme::FONT_SIZE - 1.0)
                .color(theme::dim_text(&iced::Theme::Light)),
            scrollable(list).height(Length::Fill),
            row![
                dlg_btn_auto(tr("Reset"), Some(Message::ColReset)),
                iced::widget::space::horizontal(),
                dlg_btn_primary(
                    tr("OK"),
                    app.win_of(WinKind::Columns).map(Message::CloseThis)
                ),
            ]
            .align_y(iced::Alignment::Center),
        ]
        .spacing(10)
        .padding(16),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::window)
    .into()
}
