// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! The left "Categories" tree: All Downloads with its category children,
//! Unfinished, Finished, Grabber projects, Queues.

use crate::app::{App, El, Message, TreeSel};
use crate::{i18n::tr, icons, theme};
use iced::widget::{button, column, container, row, scrollable, svg, text, text_input};
use iced::Length;

pub fn cat_icon(name: &str) -> svg::Handle {
    match name {
        crate::model::AI_CATEGORY => icons::folder_ai(),
        "Compressed" => icons::folder_compressed(),
        "Documents" => icons::folder_documents(),
        "Music" => icons::folder_music(),
        "Programs" => icons::folder_programs(),
        "Video" => icons::folder_video(),
        _ => icons::folder_custom(),
    }
}

fn node<'a>(
    app: &App,
    indent: u16,
    expander: Option<(u8, bool)>,
    icon: svg::Handle,
    label: String,
    sel: TreeSel,
) -> El<'a> {
    let selected = app.tree_sel == sel;
    let mut r = row![]
        .spacing(4)
        .align_y(iced::Alignment::Center)
        .padding(iced::Padding {
            left: (indent * 18) as f32 + 4.0,
            ..iced::Padding::ZERO
        });
    r = match expander {
        Some((idx, open)) => r.push(
            button(svg(icons::expander(open)).width(12.0).height(12.0))
                .padding(1)
                .style(theme::btn_toolbar)
                .on_press(Message::TreeToggle(idx)),
        ),
        None => r.push(iced::widget::space::horizontal().width(14.0)),
    };
    r = r
        .push(svg(icon).width(17.0).height(17.0))
        .push(text(label).size(theme::FONT_SIZE));
    button(r)
        .padding([2, 2])
        .width(Length::Fill)
        .style(theme::btn_row(selected))
        .on_press(Message::TreeSelect(sel))
        .into()
}

/// A queue row: like [`node`], but a user-created queue also answers a
/// double-click by switching into an inline rename field. Whether the row
/// may rename at all is decided by the queue's `builtin` flag in the
/// `TreeQueueRenameStart` handler, so the two stock queues (Main download
/// queue / Synchronization queue) simply never enter the edit state.
fn queue_node<'a>(app: &App, name: &str) -> El<'a> {
    let sel = TreeSel::Queue(name.to_string());
    let selected = app.tree_sel == sel;
    let pad = iced::Padding {
        left: 2.0 * 18.0 + 4.0,
        ..iced::Padding::ZERO
    };
    if app.renaming_queue.as_deref() == Some(name) {
        return container(
            row![
                iced::widget::space::horizontal().width(14.0),
                svg(icons::queue_folder(app.queue_color(name)))
                    .width(17.0)
                    .height(17.0),
                text_input("", &app.queue_rename_draft)
                    .id("tree-queue-rename")
                    .on_input(Message::TreeQueueRenameDraft)
                    .on_submit(Message::TreeQueueRenameCommit)
                    .size(theme::FONT_SIZE)
                    .style(theme::input)
                    .width(Length::Fill),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center)
            .padding(pad),
        )
        .padding([2, 2])
        .width(Length::Fill)
        .into();
    }
    let r = row![
        iced::widget::space::horizontal().width(14.0),
        svg(icons::queue_folder(app.queue_color(name)))
            .width(17.0)
            .height(17.0),
        text(tr(name)).size(theme::FONT_SIZE),
    ]
    .spacing(4)
    .align_y(iced::Alignment::Center)
    .padding(pad);
    // The double-click that starts a rename is timed in the `TreeSelect`
    // handler rather than by a `mouse_area`: this button captures the mouse
    // event, so an enclosing area's `on_double_click` would never be reached.
    button(r)
        .padding([2, 2])
        .width(Length::Fill)
        .style(theme::btn_row(selected))
        .on_press(Message::TreeSelect(sel))
        .into()
}

pub fn view(app: &App) -> El<'_> {
    let mut col = column![].spacing(1).width(Length::Fill);

    // Header strip: "Categories" + close.
    col = col.push(
        container(
            row![
                text(tr("Categories")).size(theme::FONT_SIZE),
                iced::widget::space::horizontal(),
                button(text("×").size(theme::FONT_SIZE))
                    .padding([0, 6])
                    .style(theme::btn_toolbar)
                    .on_press(Message::Menu(crate::app::MenuAction::HideCategories)),
            ]
            .align_y(iced::Alignment::Center),
        )
        .padding([2, 6])
        .width(Length::Fill),
    );

    col = col.push(node(
        app,
        0,
        Some((0, app.tree_open[0])),
        icons::folder_all(),
        tr("All Downloads"),
        TreeSel::All,
    ));
    if app.tree_open[0] {
        for c in app
            .cfg
            .categories
            .iter()
            .filter(|c| c.name != crate::model::DEFAULT_CATEGORY)
        {
            col = col.push(node(
                app,
                2,
                None,
                cat_icon(&c.name),
                tr(&c.name),
                TreeSel::Cat(c.name.clone()),
            ));
        }
    }
    col = col.push(node(
        app,
        0,
        Some((1, app.tree_open[1])),
        icons::folder_unfinished(),
        tr("Unfinished"),
        TreeSel::Unfinished,
    ));
    if app.tree_open[1] {
        for c in app
            .cfg
            .categories
            .iter()
            .filter(|c| c.name != crate::model::DEFAULT_CATEGORY)
        {
            col = col.push(node(
                app,
                2,
                None,
                cat_icon(&c.name),
                tr(&c.name),
                TreeSel::UnfCat(c.name.clone()),
            ));
        }
    }
    col = col.push(node(
        app,
        0,
        Some((2, app.tree_open[2])),
        icons::folder_finished(),
        tr("Finished"),
        TreeSel::Finished,
    ));
    if app.tree_open[2] {
        for c in app
            .cfg
            .categories
            .iter()
            .filter(|c| c.name != crate::model::DEFAULT_CATEGORY)
        {
            col = col.push(node(
                app,
                2,
                None,
                cat_icon(&c.name),
                tr(&c.name),
                TreeSel::FinCat(c.name.clone()),
            ));
        }
    }
    col = col.push(node(
        app,
        0,
        Some((3, app.tree_open[3])),
        icons::queues(),
        tr("Queues"),
        TreeSel::Queues,
    ));
    if app.tree_open[3] {
        for q in &app.cfg.queues {
            col = col.push(queue_node(app, &q.name));
        }
    }

    container(scrollable(col).height(Length::Fill))
        .width(230.0)
        .height(Length::Fill)
        .padding(2)
        .style(theme::panel)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// At 17px the tree is read by glyph before label, so every stock
    /// category has to keep its own — and a category the user made must
    /// still look like a category, not like the generic file page the
    /// fallback used to draw.
    #[test]
    fn every_stock_category_keeps_its_own_icon_and_a_custom_one_gets_a_folder() {
        let stock = [
            crate::model::AI_CATEGORY,
            "Compressed",
            "Documents",
            "Music",
            "Programs",
            "Video",
        ];
        for (i, a) in stock.iter().enumerate() {
            for b in &stock[i + 1..] {
                assert_ne!(cat_icon(a), cat_icon(b), "{a} and {b} share a glyph");
            }
        }
        assert_eq!(cat_icon("Pictures"), icons::folder_custom());
        assert_ne!(cat_icon("Pictures"), icons::file_generic());
    }
}
