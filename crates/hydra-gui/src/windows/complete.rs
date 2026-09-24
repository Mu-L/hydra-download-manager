// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! "Download complete" dialog: file, size, where it went; Open / Open folder /
//! Move/Rename.

use crate::app::{App, El, Message, WinKind};
use crate::windows::{dlg_btn, dlg_btn_primary};
use crate::{fmt, i18n::tr, icons, theme};
use iced::widget::{column, container, row, svg, text};
use iced::Length;

const PADDING: f32 = 16.0;
const SPACING: f32 = 10.0;
const MIN_WIDTH: f32 = 600.0;

/// The window is as wide as its button row needs in the active language:
/// four buttons fill the English dialog exactly, and "Move/Rename..." alone
/// outgrows its button by nearly 70px in Russian.
pub fn width() -> f32 {
    width_for(&labels())
}

fn width_for(labels: &[String]) -> f32 {
    // One gap more than there are buttons: the spacer before Close is spaced too.
    let row =
        labels.iter().map(|l| super::btn_width(l)).sum::<f32>() + labels.len() as f32 * SPACING;
    (row + 2.0 * PADDING).max(MIN_WIDTH)
}

fn labels() -> [String; 4] {
    [
        tr("Open"),
        tr("Open folder"),
        tr("Move/Rename..."),
        tr("Close"),
    ]
}

pub fn view(app: &App, id: crate::model::DlId) -> El<'_> {
    let Some(d) = app.item(id) else {
        return container(text("")).style(theme::window).into();
    };
    let [open, open_folder, move_rename, close] = labels();
    let body = column![
        row![
            svg(icons::folder_finished()).width(34.0).height(34.0),
            column![
                text(tr("Download complete")).size(theme::FONT_SIZE + 2.0),
                text(d.file_name.clone()).size(theme::FONT_SIZE),
                text(format!(
                    "{}  —  {}",
                    d.size.map(fmt::size2).unwrap_or_default(),
                    d.full_path().to_string_lossy()
                ))
                .size(theme::FONT_SIZE - 1.0)
                .color(theme::dim_text(&iced::Theme::Light)),
            ]
            .spacing(4),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center),
        iced::widget::space::vertical(),
        row![
            dlg_btn_primary(open, Some(Message::OpenFile(id))),
            dlg_btn(open_folder, Some(Message::OpenFolder(id))),
            dlg_btn(move_rename, Some(Message::MoveRename(id))),
            iced::widget::space::horizontal(),
            dlg_btn(
                close,
                app.win_of(WinKind::Complete(id)).map(Message::CloseThis)
            ),
        ]
        .spacing(SPACING),
    ]
    .spacing(10)
    .padding(PADDING);

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::window)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(labels: [&str; 4]) -> [String; 4] {
        labels.map(str::to_string)
    }

    /// No test loads a catalogue, so `tr` answers in English here.
    #[test]
    fn the_english_dialog_keeps_its_size() {
        assert_eq!(width(), MIN_WIDTH);
    }

    /// The widest shipped row. At a fixed 600px its last button was drawn
    /// past the window's edge.
    #[test]
    fn a_long_translation_widens_the_dialog_instead_of_clipping_close() {
        let russian = row([
            "Открыть",
            "Открыть папку",
            "Переместить/Переименовать...",
            "Закрыть",
        ]);
        let buttons: f32 = russian.iter().map(|l| super::super::btn_width(l)).sum();
        let w = width_for(&russian);
        assert!(w > MIN_WIDTH);
        assert!(
            w >= buttons + 4.0 * SPACING + 2.0 * PADDING,
            "Close still overflows a {w}px dialog"
        );
    }
}
