//! How a window tells the desktop what application it belongs to.
//!
//! Two different things decide which picture the desktop draws, and Linux needs
//! both:
//!
//! * [`identify`] sets `WM_CLASS`, the window's "who am I" stamp. GNOME Shell
//!   resolves it against a `.desktop` file and draws *that* file's `Icon=`. This
//!   is what a GNOME dock, alt-tab and app grid actually use; `libshell` calls
//!   `meta_window_get_wm_class` and never `meta_window_get_icon`.
//! * [`attach`] sets the `_NET_WM_ICON` property, which the shell ignores but
//!   taskbars that draw the window's own icon — KDE, Xfce, Cinnamon — read.
//!
//! Neither was set, so the shell's icon-theme lookup for an unnamed window landed
//! on `application-x-executable`: the gear that made the application look
//! unbranded on Linux and only on Linux.
//!
//! # Why this is Linux-only
//!
//! The other two platforms have no such lookup to satisfy. Windows carries the
//! icon in `res/icon.rc`, embedded into the executable by `crates/qcode/build.rs`,
//! which is what the shell and the taskbar read; macOS carries it in the app
//! bundle's `Info.plist`/`.icns`. Handing gpui an image on those platforms would
//! be dead weight, so both functions below are identities there.
//!
//! # Why the bytes are embedded
//!
//! The PNG is committed under `crates/qcode/res/icons/`, the same way the Linux
//! tray icons are (see `tray/linux.rs`). Reading it from disk at startup would
//! mean guessing an install prefix that differs between a `cargo run` tree, a
//! tarball and a distro package, and failing to a gear icon in whichever of those
//! is not the one tried. `include_bytes!` cannot be in the wrong place. Note this
//! is the *fallback* path: on GNOME the steady-state source is the installed
//! `.desktop` file, which is why `crates/qcode/res/linux/qcode.desktop` exists.
//!
//! # Limits
//!
//! * **Wayland ignores [`attach`].** The field is `#[cfg_attr(feature =
//!   "wayland", allow(dead_code))]` in gpui for exactly that reason: a Wayland
//!   client cannot hand the compositor an image, so the compositor looks the
//!   application up by `app_id` — which is what [`identify`] supplies. Wayland is
//!   covered by the `.desktop` file and the icons installed into the theme, not by
//!   this property.
//! * One size only. `_NET_WM_ICON` is normally written with several sizes
//!   concatenated so the window manager can pick; the gpui backend writes the
//!   single image it is given. 256px is the largest the icon set ships and scales
//!   down cleanly to a 48px dock slot at 2× scale, so it is the least bad single
//!   choice.

use crate::*;
#[cfg(target_os = "linux")]
use std::sync::{Arc, OnceLock};

/// The application id: the `.desktop` file's basename and the `WM_CLASS` class.
///
/// The two have to agree, and they have to agree with the icon name, or the shell
/// resolves the window to nothing and falls back to the gear. `qcode` is what the
/// icon files and the tray's StatusNotifierItem id already use.
#[cfg(target_os = "linux")]
pub(crate) const APP_ID: &str = "qcode";

/// The largest size the icon set ships, embedded so no install layout matters.
#[cfg(target_os = "linux")]
const APP_ICON_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../qcode/res/icons/linux/hicolor/256x256/apps/qcode.png"
));

/// Stamp the window with the application id, which the shell resolves to a
/// `.desktop` file and thus to an icon.
///
/// Every window goes through this rather than the one main window: the setup and
/// settings windows are separate top-level windows, so they get their own dock
/// entry and their own slot in the window switcher, and an unbranded gear there
/// would be the same bug in a smaller place.
#[cfg(target_os = "linux")]
pub(crate) fn identify(window: &mut Window) {
    window.set_app_id(APP_ID);
}

/// Identity: neither platform reads a window-supplied application id.
#[cfg(not(target_os = "linux"))]
pub(crate) fn identify(_window: &mut Window) {}

/// Attach the application icon to `options`, where the platform can use one.
#[cfg(target_os = "linux")]
pub(crate) fn attach(mut options: WindowOptions) -> WindowOptions {
    options.icon = app_icon();
    options
}

/// Identity: the platform already has an icon, and this field would be ignored.
#[cfg(not(target_os = "linux"))]
pub(crate) fn attach(options: WindowOptions) -> WindowOptions {
    options
}

/// Decode the embedded PNG once and share it across every window.
///
/// `None` on a decode failure rather than a panic: an icon is not worth taking
/// the application down for, and the window is still perfectly usable without one
/// — it just falls back to what the platform shows for an unnamed window, which is
/// where this started.
#[cfg(target_os = "linux")]
fn app_icon() -> Option<Arc<image::RgbaImage>> {
    /// `None` is cached too: a corrupt icon should fail once, not once per window.
    static DECODED: OnceLock<Option<Arc<image::RgbaImage>>> = OnceLock::new();
    DECODED
        .get_or_init(|| {
            Some(Arc::new(
                image::load_from_memory(APP_ICON_PNG).ok()?.to_rgba8(),
            ))
        })
        .clone()
}

#[cfg(all(test, target_os = "linux"))]
mod probes {
    // Narrow imports on purpose: `use super::*` here globs the crate root, and
    // that is enough extra name resolution to push this crate's `#[test]`
    // expansion past the recursion limit.
    use super::app_icon;
    use super::attach;
    use gpui_kit::WindowOptions;

    /// The embedded PNG has to decode, and to decode *square*: the window manager
    /// scales a `_NET_WM_ICON` entry without preserving aspect ratio, so a
    /// non-square image would arrive stretched and nothing else would catch it.
    #[test]
    fn the_embedded_window_icon_decodes_and_is_square() {
        let icon = app_icon().expect("the committed PNG must decode");
        assert_eq!(
            icon.width(),
            icon.height(),
            "_NET_WM_ICON is scaled without preserving aspect ratio"
        );
        assert_eq!(icon.width(), 256, "the largest size the icon set ships");
    }

    /// It is the *brand* mark, not a blank or fully transparent image: an empty
    /// canvas would decode fine and draw nothing, which looks exactly like the
    /// bug being fixed.
    #[test]
    fn the_embedded_window_icon_is_not_blank() {
        let icon = app_icon().expect("the committed PNG must decode");
        let opaque = icon.pixels().filter(|pixel| pixel.0[3] > 0).count();
        assert!(
            opaque > (icon.width() * icon.height() / 10) as usize,
            "only {opaque} of {} pixels are drawn; that is not the mark",
            icon.width() * icon.height()
        );
    }

    /// `attach` must actually reach the field — the whole bug was an `icon` that
    /// stayed `None` while everything around it looked right.
    #[test]
    fn attach_sets_the_icon() {
        let options = attach(WindowOptions::default());
        assert!(options.icon.is_some(), "attach left the icon unset");
    }

    /// The three names that have to agree, or the shell resolves the window to
    /// nothing and draws the gear again: the id the window advertises, the icon
    /// name the desktop entry declares, and the desktop entry's own basename.
    /// Changing one without the others is silent, so pin all three here.
    #[test]
    fn the_desktop_entry_agrees_with_the_app_id() {
        use super::APP_ID;
        let entry = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../qcode/res/linux/qcode.desktop"
        ));

        let key = |name: &str| {
            entry
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .unwrap_or_default()
                .trim()
        };

        assert_eq!(key("Icon"), APP_ID, "Icon= must name the installed PNGs");
        assert_eq!(
            key("StartupWMClass"),
            APP_ID,
            "StartupWMClass= must match the WM_CLASS the window sets"
        );
        assert_eq!(key("Type"), "Application");
        assert_eq!(
            key("Exec").split_whitespace().next(),
            Some("qc"),
            "the installer rewrites the path but not the subcommand"
        );
        assert_eq!(
            key("Exec").split_whitespace().nth(1),
            Some("gui"),
            "the entry has to open the GUI, not the TUI"
        );
        // Neither key may name a program that might not exist, or GLib drops the
        // whole file — and a dropped file shows up as the fallback icon, not as a
        // launch error. The installer rewrites both together for that reason; this
        // pins the shipped values to the same program so the pair cannot drift.
        assert_eq!(
            key("TryExec"),
            key("Exec").split_whitespace().next().unwrap_or_default(),
            "TryExec must name the same program as Exec"
        );
    }
}
