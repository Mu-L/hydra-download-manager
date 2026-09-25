// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Two dialogs sharing one state, laid out "Download File Info":
//! a narrow label column, one Save As box holding the whole path with a
//! "..." save dialog beside it, the folder the tick would remember shown
//! greyed under the checkbox, the file-type icon and size in a side column
//! centred beside the fields, and the buttons on a bar across the foot.
//!
//! * New download — "Download File Info": category/save-as/description while
//!   the transfer already runs in the background.
//! * Existing download — "File Properties": what the entry already is
//!   (status, size, last try and the error of the last attempt) as a block
//!   at the top, then the editable rows — Address (switch mirrors and
//!   CONTINUE the same bytes), category, path, description, login/password
//!   and cookies.

use crate::app::{App, El, FileInfoState, Message};
use crate::model::{DlState, ProxyPick};
use crate::windows::{check, dlg_btn, dlg_btn_auto, dlg_btn_primary};
use crate::{fmt, i18n::tr, theme};
use iced::widget::{button, column, container, pick_list, row, svg, text, text_input};
use iced::Length;

/// The label column's floor, matching IDM's at this font size; a locale
/// whose labels are longer widens it (see [`form`]).
const LABEL_W: f32 = 84.0;
const GAP: f32 = 8.0;

/// Half these labels reach the catalogue with a trailing colon and half
/// without — the bare ones are keys other windows share, and a translation
/// may punctuate either way — so the column decides, not the string.
fn punctuated(label: &str) -> String {
    format!("{}:", label.trim_end().trim_end_matches(':').trim_end())
}

/// The form, over one label column wide enough for the labels this locale
/// produced. A row with no label starts at the field column.
fn form<'a>(rows: Vec<(Option<String>, El<'a>)>) -> El<'a> {
    let rows = rows
        .into_iter()
        .map(|(label, content)| (label.as_deref().map(punctuated), content))
        .collect();
    crate::windows::label_column(rows, LABEL_W, GAP)
}

fn field<'a>(value: &str, on_input: fn(String) -> Message) -> text_input::TextInput<'a, Message> {
    text_input("", value)
        .on_input(on_input)
        .size(theme::FONT_SIZE)
        .style(theme::input)
        .width(Length::Fill)
}

/// small square "..." button beside Save As.
fn small_btn<'a>(label: &'a str, msg: Message) -> El<'a> {
    button(
        text(label)
            .size(theme::FONT_SIZE)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .padding([3, 9])
    .style(theme::btn)
    .on_press(msg)
    .into()
}

/// The proxy this one download takes: the app-wide setting, none, or its own
/// address. The address box is offered only for the third, and keeps its text
/// when the picker moves off it so switching back does not mean retyping.
fn proxy_row(st: &FileInfoState) -> El<'_> {
    let mut r = row![pick_list(
        &ProxyPick::ALL[..],
        Some(st.proxy_pick),
        Message::FiProxyPick
    )
    .text_size(theme::FONT_SIZE)
    .style(theme::picker)
    .width(170.0)]
    .spacing(GAP)
    .align_y(iced::Alignment::Center);
    if st.proxy_pick == ProxyPick::Custom {
        r = r.push(
            text_input("socks5://127.0.0.1:10808", &st.proxy_spec)
                .on_input(Message::FiProxySpec)
                .size(theme::FONT_SIZE)
                .style(theme::input)
                .width(Length::Fill),
        );
    }
    r.into()
}

/// Why the typed address is not a proxy, while it is being typed. Saying it
/// here is the difference between a corrected typo and a download that fails
/// minutes later with the same sentence.
///
/// `pub(crate)` because the window is sized to the rows it draws and this one
/// is conditional: see `App::window_size`.
pub(crate) fn proxy_problem(st: &FileInfoState) -> Option<String> {
    if st.proxy_pick == ProxyPick::Custom && st.proxy_spec.trim().is_empty() {
        return st
            .proxy_needs_address
            .then(|| tr("Enter the proxy address, for example socks5://127.0.0.1:10808"));
    }
    crate::proxy::typed_spec_error(st.proxy_pick, &st.proxy_spec)
}

/// What the start button does is the state's, so its caption is too:
/// "Start Download" over a finished file reads as "open it", and over a
/// half-transferred one it hides that the bytes already on disk are kept.
fn start_label(state: Option<DlState>, downloaded: u64) -> String {
    match state {
        // Already running behind the dialog: pressing it reveals the box.
        Some(s) if s.is_active() => tr("Show Progress"),
        Some(DlState::Complete) => tr("Start Download As New"),
        // Held bytes are what makes the next start a resume; a queued item,
        // and a failure that never wrote any, have nothing to continue from.
        Some(DlState::Paused | DlState::Error) if downloaded > 0 => tr("Resume Download"),
        _ => tr("Start Download"),
    }
}

pub fn view(app: &App) -> El<'_> {
    let st = &app.file_info;
    let item = app.item(st.dl);
    let size = item.and_then(|d| d.size);
    let cats: Vec<String> = app.cfg.categories.iter().map(|c| c.name.clone()).collect();
    let complete = item.map(|d| d.state == DlState::Complete).unwrap_or(false);

    let mut rows: Vec<(Option<String>, El<'_>)> = vec![];

    // What the entry already is, in one block: nothing here is editable,
    // and reading the outcome of the last attempt next to the status beats
    // hunting for it under the fields that change it.
    if !st.is_new {
        rows.push((
            Some(tr("Status:")),
            text(item.map(|d| d.status_text()).unwrap_or_default())
                .size(theme::FONT_SIZE)
                .into(),
        ));
        rows.push((
            Some(tr("Size:")),
            text(
                size.map(|s| format!("{} ({s} Bytes)", fmt::size2(s)))
                    .unwrap_or_else(|| "?".into()),
            )
            .size(theme::FONT_SIZE)
            .into(),
        ));
        rows.push((
            Some(tr("Last try date:")),
            text(
                item.and_then(|d| d.last_try)
                    .map(fmt::date)
                    .unwrap_or_default(),
            )
            .size(theme::FONT_SIZE)
            .into(),
        ));
        if let Some(err) = item.and_then(|d| d.error.clone()) {
            rows.push((
                Some(tr("Result:")),
                text(err)
                    .size(theme::FONT_SIZE)
                    .color(iced::Color::from_rgb8(0xC0, 0x2B, 0x2B))
                    .into(),
            ));
        }
    }

    // Address: read-only while the fresh probe is running, editable on an
    // existing entry — that edit is the mirror switch.
    let addr: El<'_> = if st.is_new {
        text_input("", &st.url)
            .size(theme::FONT_SIZE)
            .style(theme::input)
            .width(Length::Fill)
            .into()
    } else {
        field(&st.url, Message::FiUrl).into()
    };
    rows.push((
        Some(tr("Address:")),
        crate::windows::ext_hint(addr, &st.url),
    ));

    rows.push((
        Some(tr("Category")),
        pick_list(cats, Some(st.category.clone()), Message::FiCategory)
            .text_size(theme::FONT_SIZE)
            .style(theme::picker)
            .width(200.0)
            .into(),
    ));
    rows.push((
        Some(tr("Save As")),
        row![
            field(&st.save_as(), Message::FiSaveAs),
            small_btn("...", Message::FiBrowse),
        ]
        .spacing(GAP)
        .align_y(iced::Alignment::Center)
        .into(),
    ));
    if st.is_new {
        // Name the folder the tick actually writes: General while
        // category folders are switched off (see FiStartDownload).
        let remember_cat = if app.cfg.settings.no_category_dirs {
            app.cfg
                .categories
                .first()
                .map(|c| c.name.as_str())
                .unwrap_or(st.category.as_str())
        } else {
            st.category.as_str()
        };
        rows.push((
            None,
            check(
                st.remember,
                format!(
                    "{} \"{}\" {}",
                    tr("Remember this path for"),
                    remember_cat,
                    tr("category")
                ),
            )
            .on_toggle(Message::FiRemember)
            .into(),
        ));
        // The folder that tick would store, greyed out.
        rows.push((
            None,
            text_input("", &st.save_dir)
                .size(theme::FONT_SIZE)
                .style(|t, s| {
                    let mut style = theme::input(t, s);
                    style.value = theme::dim_text(t);
                    style
                })
                .width(Length::Fill)
                .into(),
        ));
        rows.push((
            None, // Off — and honestly so — for a file type that is not in
            // Options > File types: nothing is running behind this dialog.
            check(
                app.cfg.settings.bg_download && !st.bg_blocked,
                tr("Download in background while choosing options"),
            )
            .on_toggle(Message::FiBgToggle)
            .into(),
        ));
    }
    rows.push((
        Some(tr("Description")),
        field(&st.description, Message::FiDescription).into(),
    ));

    if !st.is_new {
        rows.push((
            Some(tr("Login")),
            row![
                text_input("", &st.login)
                    .on_input(Message::FiLogin)
                    .size(theme::FONT_SIZE)
                    .style(theme::input)
                    .width(Length::Fill),
                text(tr("Password")).size(theme::FONT_SIZE),
                text_input("", &st.password)
                    .on_input(Message::FiPass)
                    .secure(true)
                    .size(theme::FONT_SIZE)
                    .style(theme::input)
                    .width(Length::Fill),
            ]
            .spacing(GAP)
            .align_y(iced::Alignment::Center)
            .into(),
        ));
        rows.push((
            Some(tr("Cookies")),
            text_input("name=value; name2=value2", &st.cookies)
                .on_input(Message::FiCookies)
                .size(theme::FONT_SIZE)
                .style(theme::input)
                .width(Length::Fill)
                .into(),
        ));
        // Where the session came from, never a second copy of it. A download
        // that works when nothing else does is carrying a bearer credential,
        // and the user is entitled to know whether it was typed, captured by
        // the extension, or read out of a browser profile.
        if let Some(src) = app.item(st.dl).and_then(|d| d.cookie_source.clone()) {
            rows.push((
                None,
                text(src)
                    .size(theme::FONT_SIZE - 1.0)
                    .color(theme::dim_text(&iced::Theme::Light))
                    .into(),
            ));
        }
    }
    rows.push((Some(tr("Proxy")), proxy_row(st)));
    if let Some(why) = proxy_problem(st) {
        rows.push((
            None,
            text(why)
                .size(theme::FONT_SIZE - 1.0)
                .color(iced::Color::from_rgb8(0xC0, 0x2B, 0x2B))
                .into(),
        ));
    }

    // Side column: file-type icon with the size under it and the Preview
    // button beneath, centred on the fields it belongs to rather than
    // hanging off the top or bottom of them. Preview is live for an archive
    // whose index can be read off its tail (see `engine::peek_zip`), greyed
    // for everything else so the column keeps one shape.
    let previewable = hya_net::zipdir::is_zip_name(&st.file_name);
    let side = column![
        svg(crate::ui::categories::cat_icon(&st.category))
            .width(48.0)
            .height(48.0),
        text(size.map(fmt::size2).unwrap_or_else(|| "?".into())).size(theme::FONT_SIZE),
        iced::widget::space::vertical().height(6.0),
        button(crate::windows::centered(tr("Preview"), theme::FONT_SIZE))
            .padding([4, 8])
            .width(88.0)
            .style(theme::btn)
            .on_press_maybe(previewable.then_some(Message::FiPreview)),
    ]
    .spacing(6)
    .align_x(iced::Alignment::Center)
    .width(96.0);
    let side = container(side)
        .height(Length::Fill)
        .align_y(iced::Alignment::Center);

    let buttons: El<'_> = if st.is_new {
        row![
            dlg_btn(tr("Download Later"), Some(Message::FiDownloadLater)),
            dlg_btn_primary(tr("Start Download"), Some(Message::FiStartDownload)),
            dlg_btn(tr("Cancel"), Some(Message::FiCancel)),
        ]
        .spacing(GAP)
        .into()
    } else {
        row![
            dlg_btn(tr("Open"), complete.then_some(Message::OpenFile(st.dl))),
            // Sized to its caption: the state labels are far too long for the
            // uniform width the fixed buttons share.
            dlg_btn_auto(
                start_label(item.map(|d| d.state), item.map_or(0, |d| d.downloaded)),
                Some(Message::FiStartDownload),
            ),
            dlg_btn_primary(tr("OK"), Some(Message::FiOk)),
        ]
        .spacing(GAP)
        .into()
    };

    // No in-window heading: the OS title bar already names the dialog.
    //
    // The buttons get a bar of their own across the foot rather than a row
    // inside the form: the fields keep the full width beside the icon
    // column, the buttons stay centred on the dialog instead of on whatever
    // the icon column leaves, and any height the window has over the form
    // opens above them — never as a dead strip under the last button.
    container(
        column![
            row![form(rows), side].spacing(GAP).height(Length::Fill),
            row![
                iced::widget::space::horizontal(),
                buttons,
                iced::widget::space::horizontal(),
            ],
        ]
        .spacing(GAP)
        .padding(14),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .style(theme::window)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_label_ends_in_exactly_one_colon() {
        // "Address:" and "Category" are both catalogue keys as they stand,
        // so the column meets both spellings; a locale that spaces its
        // colon off the word must not come out doubled either.
        assert_eq!(punctuated("Category"), "Category:");
        assert_eq!(punctuated("Address:"), "Address:");
        assert_eq!(punctuated("Last try date :"), "Last try date:");
    }

    /// The caption has to name what the press does, because the three cases
    /// do different things to the bytes on disk: a queued item starts, a
    /// part-transferred one continues, and a finished one is fetched again
    /// over the file that is already there.
    #[test]
    fn the_start_button_is_captioned_by_the_state_it_is_pressed_in() {
        assert_eq!(start_label(Some(DlState::Queued), 0), "Start Download");
        assert_eq!(start_label(Some(DlState::Paused), 4096), "Resume Download");
        assert_eq!(
            start_label(Some(DlState::Complete), 4096),
            "Start Download As New"
        );
        // A failure that stopped mid-file still has bytes to continue from;
        // one that never got any is a plain start, not a "resume" that would
        // silently begin at zero.
        assert_eq!(start_label(Some(DlState::Error), 4096), "Resume Download");
        assert_eq!(start_label(Some(DlState::Error), 0), "Start Download");
        // A paused item can hold no bytes either — the engine was stopped
        // before the first range landed.
        assert_eq!(start_label(Some(DlState::Paused), 0), "Start Download");
        // Running: the press reveals the box rather than starting anything.
        assert_eq!(start_label(Some(DlState::Receiving), 4096), "Show Progress");
        assert_eq!(start_label(Some(DlState::Connecting), 0), "Show Progress");
        // The row was deleted from under the dialog.
        assert_eq!(start_label(None, 0), "Start Download");
    }

    /// Both shapes of the proxy row build — the picker alone, and the picker
    /// with the address box — and a widget tree that does not survive being
    /// built takes the whole dialog down with it.
    #[test]
    fn both_shapes_of_the_proxy_row_lay_out() {
        let plain = FileInfoState::default();
        let _default: El<'_> = proxy_row(&plain);
        let own = FileInfoState {
            proxy_pick: ProxyPick::Custom,
            proxy_spec: "socks5://127.0.0.1:10808".into(),
            ..FileInfoState::default()
        };
        let _custom: El<'_> = proxy_row(&own);
    }

    /// An unusable address is reported only while one is being asked for:
    /// text left in the box under "Default" is not an error to shout about.
    #[test]
    fn a_bad_address_is_reported_only_while_it_is_being_asked_for() {
        let bad = FileInfoState {
            proxy_pick: ProxyPick::Custom,
            proxy_spec: "gopher://p".into(),
            ..FileInfoState::default()
        };
        assert!(proxy_problem(&bad).is_some());
        assert_eq!(
            proxy_problem(&FileInfoState {
                proxy_pick: ProxyPick::Default,
                ..bad.clone()
            }),
            None
        );
        assert_eq!(
            proxy_problem(&FileInfoState {
                proxy_spec: "socks5://127.0.0.1:10808".into(),
                ..bad.clone()
            }),
            None
        );
        // An address not yet typed is not a mistake to point at — until OK
        // or Start Download is pressed with the box still empty.
        let blank = FileInfoState {
            proxy_spec: String::new(),
            ..bad
        };
        assert_eq!(proxy_problem(&blank), None);
        let asked = FileInfoState {
            proxy_needs_address: true,
            ..blank
        };
        let why = proxy_problem(&asked).expect("a refused commit says what is missing");
        assert!(
            why.contains("socks5://"),
            "the message has to show what an address looks like: {why}"
        );
    }
}
