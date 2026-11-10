use cosmic::widget::icon;
use freedesktop_desktop_entry::{DesktopEntry, Iter as DesktopIter};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Rendered when an app's icon can't be resolved. libcosmic substitutes an *empty* SVG for
/// a name that isn't in the theme, so without this a window in the strip renders as a blank
/// cell and reads as a window that's missing entirely (#9).
const FALLBACK_ICON: &str = "application-x-executable";

/// What to draw for one window.
#[derive(Clone)]
pub enum AppIcon {
    /// A name to look up in the icon theme. Always one that resolved.
    Named(String),
    /// An icon file named outright by a `.desktop` entry, as Snap and some AppImage
    /// entries do. Those paths are outside any theme, so a name lookup can't find them.
    File(PathBuf),
}

/// How one window is presented in the strip.
#[derive(Clone)]
pub struct AppVisual {
    /// Resolved icon handle.  Keeping the handle id stable across views lets
    /// the SVG raster cache hit; rebuilding it from the name re-rasterizes the
    /// icon on every frame.
    pub handle: icon::Handle,
    /// The `.desktop` entry's Name=, used only when the window title is empty.
    pub label: Option<String>,
    /// Kept for the icon-resolution tests; production code never reads it.
    #[cfg(test)]
    pub icon: AppIcon,
}

fn visual(icon: AppIcon, label: Option<String>, size: u16) -> AppVisual {
    let handle = match &icon {
        AppIcon::Named(name) => icon::from_name(name.as_str()).size(size).handle(),
        AppIcon::File(path) => icon::from_path(path.clone()),
    };
    AppVisual {
        #[cfg(test)]
        icon,
        handle,
        label,
    }
}

/// The label to print for a window: its own title first, then the desktop
/// entry's name, then the app id.
pub fn window_label<'a>(app_id: &'a str, window_title: &'a str, visual: &'a AppVisual) -> &'a str {
    if !window_title.is_empty() {
        window_title
    } else if let Some(label) = &visual.label {
        label
    } else if !app_id.is_empty() {
        app_id
    } else {
        "Unknown"
    }
}

/// Resolves what to draw for `app_id`, at the size the icon will be drawn at: a name can
/// resolve at one size and not another, so the check has to use the size we'll render.
pub fn visual_for(app_id: &str, size: u16) -> AppVisual {
    visual_for_in(app_id, size, &search_dirs())
}

fn visual_for_in(app_id: &str, size: u16, dirs: &[PathBuf]) -> AppVisual {
    // Steam games: the per-game icon isn't reliably resolvable, so use Steam's own.
    // The window title is the game's name, which beats every other label we could pick.
    if app_id.starts_with("steam_app_") {
        return visual(resolve("steam", size), Some("Steam".to_string()), size);
    }

    let entry = desktop_entry_for(app_id, dirs);
    let label = entry
        .as_ref()
        .and_then(|e| e.name.clone())
        .filter(|name| !name.is_empty());

    let icon = match entry.and_then(|e| e.icon) {
        // An absolute Icon= that exists is the icon, no theme lookup involved.
        Some(field) if Path::new(&field).is_absolute() && Path::new(&field).is_file() => {
            AppIcon::File(PathBuf::from(field))
        }
        // An absolute Icon= that doesn't exist still names the icon by its stem, which is
        // how Steam's entries point at icons it installed into the theme.
        Some(field) => resolve(&icon_name_from_field(&field), size),
        // No entry found: reverse-DNS app_ids often end in their own icon name.
        None => resolve(app_id.split('.').next_back().unwrap_or(app_id), size),
    };

    visual(icon, label, size)
}

/// Falls back to the generic icon if `name` isn't in the theme.
fn resolve(name: &str, size: u16) -> AppIcon {
    if icon::from_name(name).size(size).path().is_some() {
        return AppIcon::Named(name.to_string());
    }
    log::debug!("icon {name:?} not found at size {size}, falling back to {FALLBACK_ICON}");
    AppIcon::Named(FALLBACK_ICON.to_string())
}

/// The fields we use from a `.desktop` entry.
struct DesktopInfo {
    icon: Option<String>,
    name: Option<String>,
}

/// The `.desktop` entry describing `app_id`, if one can be found.
fn desktop_entry_for(app_id: &str, dirs: &[PathBuf]) -> Option<DesktopInfo> {
    if app_id.is_empty() {
        return None;
    }

    // Direct filename match: "firefox" -> "firefox.desktop"
    for dir in dirs {
        if let Some(info) = read_entry(&dir.join(format!("{app_id}.desktop"))) {
            return Some(info);
        }
    }

    // Otherwise scan every entry for one that claims this app_id.
    for path in DesktopIter::new(dirs.to_vec()) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(s) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let Ok(entry) = DesktopEntry::decode(&path, s) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let claims = entry
            .startup_wm_class()
            .is_some_and(|c| c.eq_ignore_ascii_case(app_id))
            || stem_matches(stem, app_id);
        if claims {
            return Some(DesktopInfo {
                icon: entry.icon().map(ToString::to_string),
                name: entry.name(None).map(|n| n.to_string()),
            });
        }
    }

    log::debug!("no .desktop entry found for app_id {app_id:?}");
    None
}

/// Whether a `.desktop` filename describes `app_id`. Compositors report app_ids with
/// inconsistent case, and often report only the last component of a reverse-DNS name
/// (app_id "jdownloader" for "org.jdownloader.JDownloader.desktop").
fn stem_matches(stem: &str, app_id: &str) -> bool {
    stem.eq_ignore_ascii_case(app_id)
        || stem
            .rsplit('.')
            .next()
            .is_some_and(|last| last.eq_ignore_ascii_case(app_id))
}

fn read_entry(path: &Path) -> Option<DesktopInfo> {
    let bytes = std::fs::read(path).ok()?;
    let s = std::str::from_utf8(&bytes).ok()?;
    let entry = DesktopEntry::decode(path, s).ok()?;
    Some(DesktopInfo {
        icon: entry.icon().map(ToString::to_string),
        name: entry.name(None).map(|n| n.to_string()),
    })
}

/// Every directory that can hold `.desktop` files, in XDG precedence order.
fn search_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("{home}/.local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    search_dirs_for(&data_home, &data_dirs)
}

/// Pure form of [`search_dirs`], suitable for tests.
fn search_dirs_for(data_home: &str, data_dirs: &str) -> Vec<PathBuf> {
    let xdg = std::iter::once(data_home.to_string())
        .chain(data_dirs.split(':').map(ToString::to_string))
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("applications"));

    // Flatpak's export dirs are in XDG_DATA_DIRS only for sessions that started after the
    // first install, and Snap's is never there, so name all three outright.
    let extra = [
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        PathBuf::from(format!("{data_home}/flatpak/exports/share/applications")),
        PathBuf::from("/var/lib/snapd/desktop/applications"),
    ];

    let mut seen = HashSet::new();
    xdg.chain(extra)
        .filter(|d| seen.insert(d.clone()))
        .collect()
}

// If the Icon= field in a .desktop file is an absolute path, extract the stem
// (e.g. "/path/to/steam_icon_480.png" -> "steam_icon_480") so that from_name()
// can resolve it through the XDG icon theme where Steam has already installed it.
fn icon_name_from_field(icon: &str) -> String {
    if icon.starts_with('/') {
        Path::new(icon)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(icon)
            .to_string()
    } else {
        icon.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppIcon, FALLBACK_ICON, icon_name_from_field, search_dirs_for, stem_matches, visual_for_in,
        window_label,
    };
    use std::path::PathBuf;

    #[test]
    fn absolute_icon_fields_become_stems() {
        assert_eq!(
            icon_name_from_field("/a/b/steam_icon_480.png"),
            "steam_icon_480"
        );
        assert_eq!(icon_name_from_field("firefox"), "firefox");
    }

    #[test]
    fn stems_match_case_insensitively_and_by_last_component() {
        assert!(stem_matches("Firefox", "firefox"));
        assert!(stem_matches("org.jdownloader.JDownloader", "jdownloader"));
        assert!(!stem_matches("org.gnome.Nautilus", "gnome"));
        assert!(!stem_matches("firefox", "chromium"));
    }

    #[test]
    fn resolution_against_a_fixture_session() {
        let root = std::env::temp_dir().join(format!("switcher-icons-{}", std::process::id()));
        let data_home = root.join("data_home");
        let apps = data_home.join("applications");
        let icon = root.join("thing.png");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::write(&icon, b"not really a png").unwrap();
        std::fs::write(
            apps.join("org.example.Thing.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=Thing\nIcon={}\n",
                icon.display()
            ),
        )
        .unwrap();

        // The dirs the pre-#9 lookup used are still searched, plus Snap's.
        let dirs = search_dirs_for(&data_home.display().to_string(), "/usr/share");
        for expected in [
            PathBuf::from("/usr/share/applications"),
            apps.clone(),
            PathBuf::from("/var/lib/flatpak/exports/share/applications"),
            data_home.join("flatpak/exports/share/applications"),
            PathBuf::from("/var/lib/snapd/desktop/applications"),
        ] {
            assert!(
                dirs.contains(&expected),
                "missing {} in {dirs:?}",
                expected.display()
            );
        }
        let mut deduped = dirs.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), dirs.len(), "duplicate dirs in {dirs:?}");

        // An Icon= naming a file on disk is drawn from that file, by exact app_id and by
        // the last component of the entry's name. The label is the entry's Name=.
        for app_id in ["org.example.Thing", "thing"] {
            let visual = visual_for_in(app_id, 48, &dirs);
            match &visual.icon {
                AppIcon::File(p) => assert_eq!(p, &icon, "for app_id {app_id:?}"),
                AppIcon::Named(n) => panic!("app_id {app_id:?} resolved to name {n:?}"),
            }
            assert_eq!(
                visual.label.as_deref(),
                Some("Thing"),
                "for app_id {app_id:?}"
            );
        }

        // An app_id nothing claims falls back to the generic icon. The per-window title
        // is resolved by `window_label`, not by the cached visual.
        let visual = visual_for_in("no-such-app-6f3b", 48, &dirs);
        match &visual.icon {
            AppIcon::Named(n) => assert_eq!(n, FALLBACK_ICON),
            AppIcon::File(p) => panic!("unknown app_id resolved to file {}", p.display()),
        }
        assert!(visual.label.is_none());
        assert_eq!(
            window_label("no-such-app-6f3b", "Some Window Title", &visual),
            "Some Window Title"
        );
        assert_eq!(
            window_label("no-such-app-6f3b", "", &visual),
            "no-such-app-6f3b"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
