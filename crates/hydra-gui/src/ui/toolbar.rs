// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! The toolbar: big gradient-outline icons, with labels underneath unless
//! View > Hide toolbar text is on — then the label becomes a hover tooltip.

use crate::app::{App, El, MenuAction, Message};
use crate::model::DlState;
use crate::{i18n::tr, icons, theme};
use iced::widget::{button, column, container, row, svg, text, tooltip};
use iced::{Alignment, Length};

/// Geometry of the row, shared by `view` and [`min_width`]: the icon
/// square, the padding either side of it, the gap between tools, a split
/// button's arrow, and the padding at each end of the row.
const ICON_W: f32 = 34.0;
const TOOL_PAD_X: f32 = 10.0;
const GAP: f32 = 4.0;
const ARROW_W: f32 = 14.0;
const ROW_PAD_X: f32 = 8.0;
/// The three split buttons: Start Queue, Stop Queue, Speed Limit.
const ARROWS: usize = 3;

/// Every label the row draws, widest form (a Speed Limiter with a cap set
/// shows the cap, which is shorter than its name). `min_width` measures
/// these and a test holds the list to what `view` actually draws.
const TOOL_LABELS: [&str; 12] = [
    "Add URL",
    "Resume",
    "Stop",
    "Stop All",
    "Delete",
    "Delete Completed",
    "Options",
    "Extensions",
    "Scheduler",
    "Start Queue",
    "Stop Queue",
    "Speed Limit",
];

/// The width the toolbar needs in this locale, labels and all.
///
/// The row does not wrap or scroll: past the right edge iced squeezes the
/// last tools to nothing and draws their labels over each other. The
/// window's floor comes from here rather than from a number measured once
/// against English, which every longer translation then overran.
pub fn min_width() -> f32 {
    let tools: f32 = TOOL_LABELS
        .iter()
        .map(|l| crate::font::line_width(&tr(l), theme::FONT_SIZE).max(ICON_W) + 2.0 * TOOL_PAD_X)
        .sum();
    let n = TOOL_LABELS.len() + ARROWS;
    tools + ARROWS as f32 * ARROW_W + (n - 1) as f32 * GAP + 2.0 * ROW_PAD_X
}

fn tool<'a>(icon: svg::Handle, label: String, msg: Option<Message>, labels: bool) -> El<'a> {
    let icon = svg(icon).width(ICON_W).height(ICON_W);
    let content: El<'a> = if labels {
        column![icon, text(label.clone()).size(theme::FONT_SIZE)]
            .spacing(3)
            .align_x(Alignment::Center)
            .into()
    } else {
        icon.into()
    };
    let mut b = button(content)
        .padding([6.0, TOOL_PAD_X])
        .style(theme::btn_toolbar);
    if let Some(m) = msg {
        b = b.on_press(m);
    }
    if labels {
        return b.into();
    }
    // Without the label the icon alone has to say what the button does, so
    // the text that was under it moves to the hover hint.
    tooltip(
        b,
        container(text(label).size(theme::FONT_SIZE))
            .padding(6)
            .style(theme::menu_panel),
        tooltip::Position::Bottom,
    )
    .into()
}

/// The Speed Limiter button's text: the cap in force rather than a fixed
/// word — a limit left on by mistake is the failure mode worth making
/// visible. With the labels hidden that text is only a tooltip, where a bare
/// number names nothing, so the button's own name rides along with it.
fn speed_label(limit: Option<u64>, labels: bool) -> String {
    match limit {
        None => tr("Speed Limit"),
        Some(_) if labels => crate::fmt::limit(limit),
        Some(_) => format!(
            "{} \u{2014} {}",
            tr("Speed Limit"),
            crate::fmt::limit(limit)
        ),
    }
}

pub fn view(app: &App) -> El<'_> {
    let sel_some = !app.selected.is_empty();
    let in_sel = |d: &&crate::model::DownloadItem| app.selected.contains(&d.id);
    let sel_active = app
        .state
        .downloads
        .iter()
        .filter(in_sel)
        .any(|d| d.state.is_active());
    let sel_resumable = app
        .state
        .downloads
        .iter()
        .filter(in_sel)
        .any(|d| matches!(d.state, DlState::Paused | DlState::Error | DlState::Queued));
    let any_active = app.state.downloads.iter().any(|d| d.state.is_active());
    let any_queue_running = app.cfg.queues.iter().any(|q| q.running);
    let limit = app.cfg.settings.global_limit();
    let labels = app.cfg.settings.show_toolbar_labels;
    let main_q = app
        .cfg
        .queues
        .first()
        .map(|q| q.name.clone())
        .unwrap_or_else(|| "Main download queue".into());

    row![
        tool(
            icons::add_url(true),
            tr("Add URL"),
            Some(Message::Menu(MenuAction::AddNewDownload)),
            labels,
        ),
        tool(
            icons::resume(sel_resumable),
            tr("Resume"),
            sel_resumable.then_some(Message::ToolbarResume),
            labels,
        ),
        tool(
            icons::stop(sel_active),
            tr("Stop"),
            sel_active.then_some(Message::ToolbarStop),
            labels,
        ),
        tool(
            icons::stop_all(any_active),
            tr("Stop All"),
            any_active.then_some(Message::Menu(MenuAction::StopAll)),
            labels,
        ),
        tool(
            icons::delete(sel_some),
            tr("Delete"),
            sel_some.then_some(Message::ToolbarDelete),
            labels,
        ),
        tool(
            icons::delete_completed(true),
            tr("Delete Completed"),
            Some(Message::Menu(MenuAction::DeleteAllCompleted)),
            labels,
        ),
        tool(
            icons::options(true),
            tr("Options"),
            Some(Message::Menu(MenuAction::Options)),
            labels,
        ),
        // Shortcut into Options > Extensions: the browser add-on is how most
        // downloads reach Hydra, so it gets a toolbar entry of its own.
        tool(
            icons::extensions(true),
            tr("Extensions"),
            Some(Message::Menu(MenuAction::Extensions)),
            labels,
        ),
        tool(
            icons::scheduler(true),
            tr("Scheduler"),
            Some(Message::Menu(MenuAction::Scheduler)),
            labels,
        ),
        // Split buttons: the big button acts on the MAIN queue,
        // the arrow opens the list of all queues.
        tool(
            icons::start_queue(!any_queue_running),
            tr("Start Queue"),
            (!any_queue_running).then_some(Message::Menu(MenuAction::StartQueue(main_q.clone()))),
            labels,
        ),
        button(text("▾").size(theme::FONT_SIZE))
            .padding([4, 3])
            .style(theme::btn_toolbar)
            .on_press(Message::QueueMenuOpen(true)),
        tool(
            icons::stop_queue(any_queue_running),
            tr("Stop Queue"),
            any_queue_running.then_some(Message::Menu(MenuAction::StopQueue(main_q))),
            labels,
        ),
        button(text("▾").size(theme::FONT_SIZE))
            .padding([4, 3])
            .style(theme::btn_toolbar)
            .on_press(Message::QueueMenuOpen(false)),
        // Speed Limiter, one click from the queue it is throttling: the big
        // button switches the cap on and off, the arrow picks a profile.
        tool(
            icons::speed_limit(limit.is_some()),
            speed_label(limit, labels),
            Some(Message::Menu(MenuAction::SpeedLimiterToggle)),
            labels,
        ),
        button(text("▾").size(theme::FONT_SIZE))
            .padding([4, 3])
            .style(theme::btn_toolbar)
            .on_press(Message::SpeedMenuOpen),
    ]
    .spacing(GAP)
    .padding([4.0, ROW_PAD_X])
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every label `view` draws, read off this file: a tool added without
    /// its label in [`TOOL_LABELS`] would leave the window's floor too
    /// narrow for the row, which is the bug this list exists to prevent.
    fn labels_drawn() -> std::collections::BTreeSet<String> {
        let src = include_str!("toolbar.rs");
        let call = ["tr", "(", "\""].concat();
        src.match_indices(&call)
            .filter_map(|(i, _)| {
                let rest = &src[i + call.len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect()
    }

    #[test]
    fn the_window_floor_measures_every_label_the_row_draws() {
        let measured: std::collections::BTreeSet<String> =
            TOOL_LABELS.iter().map(|l| l.to_string()).collect();
        assert_eq!(labels_drawn(), measured);
    }

    #[test]
    fn the_row_asks_for_at_least_what_its_tools_take() {
        // The floor has to cover the labels and the chrome around them, or
        // the last tools are squeezed to nothing and drawn over each other.
        let tools: f32 = TOOL_LABELS
            .iter()
            .map(|l| crate::font::line_width(&tr(l), theme::FONT_SIZE).max(ICON_W))
            .sum();
        assert!(min_width() > tools, "no room for the chrome around them");
        // And the window keeps the floor it has always had, so a locale
        // that never overran it sees no change.
        assert!(crate::app::main_min_w() >= 1050.0);
    }

    #[test]
    fn a_hidden_label_leaves_the_speed_cap_named_in_its_hover_hint() {
        let cap = Some(512 * 1024);
        let shown = speed_label(cap, true);
        let hint = speed_label(cap, false);
        assert_eq!(shown, "512 KB/s");
        assert_eq!(hint, "Speed Limit \u{2014} 512 KB/s");
        // No cap: the button is named the same either way.
        assert_eq!(speed_label(None, true), speed_label(None, false));
    }
}
