// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! macOS + Windows tray backend: `tray-icon` (which re-exports muda as
//! `tray_icon::menu`, so tray and macOS menu bar share one muda instance and
//! therefore one global menu-event channel).

#![cfg(any(target_os = "macos", target_os = "windows"))]

use super::Entry;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

thread_local! {
    static CURRENT: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}
/// Readable from any thread, unlike `CURRENT` — `is_active` answers whether
/// closing the last window may leave the app running.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The shared mono silhouette as a muda icon (see [`crate::icons::logo_mono_rgba`]).
fn mono_icon(white: bool) -> Option<tray_icon::Icon> {
    let (rgba, w, h) = crate::icons::logo_mono_rgba(white)?;
    tray_icon::Icon::from_rgba(rgba, w, h).ok()
}

/// Whether the notification area wants the WHITE glyph.
///
/// `SystemUsesLightTheme`, not `AppsUseLightTheme`. Windows keeps the two
/// apart — Personalization > Colors sets "Windows mode" and "App mode"
/// independently — and the tray icon lives in the TASKBAR, which follows the
/// system one. `dark_light::detect` reads the app value, so a machine with a
/// dark taskbar and light apps got a black glyph painted onto a black
/// taskbar: an icon that is there and cannot be seen.
///
/// A missing value falls back to the app theme rather than to a guess: the
/// key has shipped since Windows 10 1903, and if it is somehow absent the app
/// setting is the best evidence left.
#[cfg(target_os = "windows")]
fn taskbar_wants_white() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(THEME_SUBKEY)
        .and_then(|k| k.get_value::<u32, _>("SystemUsesLightTheme"))
        .map(|light| light == 0)
        .unwrap_or_else(|_| matches!(dark_light::detect(), Ok(dark_light::Mode::Dark)))
}

#[cfg(target_os = "windows")]
const THEME_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// Re-tint the glyph for the taskbar theme in force right now.
///
/// Called on the UI thread — `CURRENT` is thread-local — in answer to
/// [`super::THEME_CHANGED`] from the watcher below.
#[cfg(target_os = "windows")]
pub fn refresh_icon() {
    let white = taskbar_wants_white();
    CURRENT.with(|c| {
        if let Some(tray) = c.borrow().as_ref() {
            if let Some(icon) = mono_icon(white) {
                crate::log::info(&format!(
                    "tray: glyph now {}",
                    if white { "white" } else { "black" }
                ));
                let _ = tray.set_icon(Some(icon));
            }
        }
    });
}

#[cfg(not(target_os = "windows"))]
pub fn refresh_icon() {
    // macOS renders the glyph as a template image and recolors it itself.
}

/// Watch the theme key and ask the UI thread to re-tint when it changes.
///
/// Its own watch rather than `dark_light::subscribe`, which reports changes
/// to `AppsUseLightTheme` only: switching Windows mode alone leaves that
/// value untouched, so the stream stays silent through exactly the change
/// this icon cares about. `RegNotifyChangeKeyValue` fires on any value in the
/// key; deciding what actually changed is [`taskbar_wants_white`]'s job.
#[cfg(target_os = "windows")]
fn watch_taskbar_theme() {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Registry::{
        RegNotifyChangeKeyValue, REG_NOTIFY_CHANGE_LAST_SET,
    };
    use winreg::enums::{HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ};

    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    let Ok(key) = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(THEME_SUBKEY, KEY_READ | KEY_NOTIFY)
    else {
        return; // No key to watch: the startup pick stands.
    };
    let tx = crate::menubus::sender();
    let _ = std::thread::Builder::new()
        .name("hydra-tray-theme".into())
        .spawn(move || {
            let mut last = taskbar_wants_white();
            loop {
                // The handle is read from `key` on every pass so the key is
                // provably still alive at the call: closing it is what ends a
                // pending notify, and a watch registered on a closed handle
                // would return at once, forever.
                //
                // SAFETY: an open key this thread owns, a filter constant, no
                // event handle, synchronous — so the call simply blocks until
                // a value under the key is written.
                let status = unsafe {
                    RegNotifyChangeKeyValue(
                        key.raw_handle(),
                        0,
                        REG_NOTIFY_CHANGE_LAST_SET,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if status != 0 {
                    return;
                }
                let white = taskbar_wants_white();
                if white == last {
                    continue; // Some other personalization value moved.
                }
                last = white;
                if tx.send(super::THEME_CHANGED.to_string()).is_err() {
                    return;
                }
            }
        });
}

/// Render the shared menu model into muda items. Boxed because a submenu's
/// children must outlive the `append_items` call that takes them by
/// reference.
fn render(entries: &[Entry]) -> Vec<Box<dyn IsMenuItem>> {
    entries
        .iter()
        .map(|e| -> Box<dyn IsMenuItem> {
            match e {
                Entry::Separator => Box::new(PredefinedMenuItem::separator()),
                Entry::Item { id, label } => {
                    Box::new(MenuItem::with_id(id.clone(), label, true, None))
                }
                Entry::Check { id, label, checked } => Box::new(CheckMenuItem::with_id(
                    id.clone(),
                    label,
                    true,
                    *checked,
                    None,
                )),
                Entry::Sub { label, items } => {
                    let sub = Submenu::new(label, true);
                    let children = render(items);
                    let refs: Vec<&dyn IsMenuItem> = children.iter().map(|c| c.as_ref()).collect();
                    let _ = sub.append_items(&refs);
                    Box::new(sub)
                }
            }
        })
        .collect()
}

fn build_menu(entries: &[Entry]) -> Menu {
    let menu = Menu::new();
    let children = render(entries);
    let refs: Vec<&dyn IsMenuItem> = children.iter().map(|c| c.as_ref()).collect();
    let _ = menu.append_items(&refs);
    menu
}

pub fn install(entries: Vec<Entry>) {
    let installed = CURRENT.with(|c| c.borrow().is_some());
    if installed {
        return;
    }
    crate::menubus::ensure_menu_handler();
    install_with_menu(build_menu(&entries));
}

pub fn reinstall(entries: Vec<Entry>) {
    CURRENT.with(|c| {
        if let Some(tray) = c.borrow().as_ref() {
            tray.set_menu(Some(Box::new(build_menu(&entries))));
        }
    });
}

pub fn is_active() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

fn install_with_menu(menu: Menu) {
    // Left-click on the icon brings the main window back; the menu is
    // right-click only (see `with_menu_on_left_click` below), matching the
    // Linux backend's `activate`.
    let tx = crate::menubus::sender();
    TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = ev
        {
            let _ = tx.send("show_main".into());
        }
    }));

    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Hydra");
    // tray-icon pops the menu on either button by default, while the Click
    // event fires regardless — so a left-click used to raise the window
    // *behind* an unwanted menu. Left-click activates, right-click opens the
    // menu, as the Windows shell does and as ksni already does on Linux.
    builder = builder.with_menu_on_left_click(false);
    // macOS: a TEMPLATE image — black + alpha that AppKit recolors itself
    // for the light/dark menu bar (and inverts while highlighted). Windows
    // has no template concept, so the colour is picked here and re-picked by
    // `watch_taskbar_theme` whenever the taskbar's own theme changes.
    #[cfg(target_os = "macos")]
    {
        if let Some(icon) = mono_icon(false) {
            builder = builder.with_icon(icon).with_icon_as_template(true);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(icon) = mono_icon(taskbar_wants_white()) {
            builder = builder.with_icon(icon);
        }
    }
    match builder.build() {
        Ok(tray) => {
            CURRENT.with(|c| *c.borrow_mut() = Some(tray));
            INSTALLED.store(true, Ordering::Relaxed);
            // Only once there IS an icon to re-tint.
            #[cfg(target_os = "windows")]
            watch_taskbar_theme();
        }
        Err(e) => crate::log::warn(&format!("tray icon unavailable: {e}")),
    }
}
