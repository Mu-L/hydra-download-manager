// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! The face the interface is drawn with, chosen for the locale.
//!
//! Hydra draws its interface the way the machine draws its own: the platform
//! UI face, so a Hydra window looks like the windows beside it and covers the
//! scripts that platform covers. Persian and Arabic are the exception —
//! per-glyph system fallback shaped some Arabic-script runs to nothing, which
//! is why Vazirmatn ships inside the binary and stays the face those locales
//! are drawn with. It is the last resort everywhere else too, for a machine
//! that has none of the faces its platform is supposed to have.
//!
//! There is no setting for this: the language already says which face reads
//! best, and the renderer is built with its default font once, when the
//! window system comes up, so a face picked at runtime could not reach the
//! interface before the next launch anyway.

/// The bundled family, loaded from `assets/fonts` in `main`.
pub const BUNDLED: &str = "Vazirmatn";

/// The locales the bundled face is there for: the scripts whose shaping the
/// system fallback got wrong, and which Vazirmatn covers completely.
const BUNDLED_LOCALES: [&str; 2] = ["ar", "fa"];

/// The faces this platform draws its own interface with, best first.
#[cfg(target_os = "windows")]
const PLATFORM_FACES: [&str; 3] = ["Segoe UI Variable Text", "Segoe UI", "Tahoma"];
#[cfg(target_os = "macos")]
const PLATFORM_FACES: [&str; 3] = ["SF Pro Text", "SF Pro", "Helvetica Neue"];
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const PLATFORM_FACES: [&str; 6] = [
    "Cantarell",
    "Ubuntu",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
];

/// The default font for the whole interface, for the locale this session runs
/// in.
pub fn default_font(locale: Option<&str>) -> iced::Font {
    iced::Font::with_name(default_face(locale))
}

/// Whether moving from one locale to another lands the interface on a
/// different face — the question View > Language asks, because the renderer
/// was built with the face the session started on and cannot be given
/// another one until the next launch.
pub fn changes_face(from: Option<&str>, to: Option<&str>) -> bool {
    default_face(from) != default_face(to)
}

/// The bundled face where its script needs it, otherwise the first face this
/// platform draws with that the machine actually has. Never a name the text
/// system cannot resolve: an unresolvable family leaves every label to the
/// fallback chain, which is the failure the bundled face exists to avoid.
fn default_face(locale: Option<&str>) -> &'static str {
    if bundled_locale(locale) {
        return BUNDLED;
    }
    PLATFORM_FACES
        .into_iter()
        .find(|face| is_installed(face))
        .unwrap_or(BUNDLED)
}

/// Whether this locale is one the bundled face exists for. Matches on the
/// language subtag, so `fa`, `fa-IR` and a hand-written `fa_IR` all count.
fn bundled_locale(locale: Option<&str>) -> bool {
    let Some(tag) = locale else {
        return false;
    };
    let language = tag.split(['-', '_']).next().unwrap_or(tag).to_lowercase();
    BUNDLED_LOCALES.contains(&language.as_str())
}

/// Whether the text system can shape with this family — asked of the same
/// font database that resolves the name later, rather than guessed from the
/// platform. The scan it triggers is the one the first frame would pay for.
fn is_installed(family: &str) -> bool {
    let Ok(mut fonts) = iced::advanced::graphics::text::font_system().write() else {
        // A poisoned lock means text rendering has already failed somewhere;
        // the bundled face is the safe answer.
        return false;
    };
    let installed = fonts
        .raw()
        .db()
        .faces()
        .any(|face| face.families.iter().any(|(name, _)| name == family));
    // Before the answer leaves: the renderer wants this lock back.
    drop(fonts);
    installed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arabic_script_locales_keep_the_bundled_face() {
        for tag in ["fa", "fa-IR", "fa_IR", "ar", "AR"] {
            assert!(bundled_locale(Some(tag)), "{tag} lost Vazirmatn");
            assert_eq!(
                default_font(Some(tag)).family,
                iced::font::Family::Name(BUNDLED)
            );
        }
        // Everyone else follows the platform, including a session that has
        // never picked a language and the legacy "English" tag.
        for tag in [None, Some("en"), Some("English"), Some("zh"), Some("he")] {
            assert!(!bundled_locale(tag), "{tag:?} was treated as Arabic script");
        }
    }

    #[test]
    fn only_crossing_the_bundled_scripts_changes_the_face() {
        // Both sides of the boundary, either way round.
        assert!(changes_face(Some("en"), Some("fa")));
        assert!(changes_face(Some("ar"), Some("zh")));
        assert!(changes_face(None, Some("fa-IR")));

        // Two locales the same face covers do not send anyone to a dialog.
        assert!(!changes_face(Some("fa"), Some("ar")));
        assert!(!changes_face(Some("en"), Some("de")));
        assert!(!changes_face(Some("zh"), None));
    }

    #[test]
    fn the_default_face_is_one_this_machine_can_actually_shape_with() {
        let face = default_face(Some("en"));
        assert!(
            face == BUNDLED || is_installed(face),
            "{face} is neither installed nor the bundled face"
        );
        assert!(
            face == BUNDLED || PLATFORM_FACES.contains(&face),
            "{face} is not a face this platform was asked for"
        );
    }
}
