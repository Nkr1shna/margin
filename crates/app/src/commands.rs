//! App-wide commands with keyboard shortcuts. The same command arrives from the web
//! view's key handler (`MarginApp.watchShortcuts`) and, on desktop, from the menu bar,
//! and both run through `MarginApp.command` so they behave the same: ignored while a
//! modal dialog is open, and one press never runs twice.

/// A command bound to a shortcut. Its slug is what `margin.js` sends and the menu item id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// Cmd+K (Ctrl+K off Apple platforms).
    Search,
    /// Cmd+N.
    Ask,
    /// Shift+Cmd+N.
    AllNotes,
    /// Cmd+,
    Settings,
    /// Cmd+[
    Back,
    /// Cmd+]
    Forward,
    /// Cmd+\
    ToggleSidebar,
}

impl Command {
    pub const ALL: [Command; 7] = [
        Command::Search,
        Command::Ask,
        Command::AllNotes,
        Command::Settings,
        Command::Back,
        Command::Forward,
        Command::ToggleSidebar,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            Command::Search => "search",
            Command::Ask => "ask",
            Command::AllNotes => "notes",
            Command::Settings => "settings",
            Command::Back => "back",
            Command::Forward => "forward",
            Command::ToggleSidebar => "sidebar",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.slug() == slug)
    }

    /// The key, as the shortcut label shows it.
    pub fn key(self) -> &'static str {
        match self {
            Command::Search => "K",
            Command::Ask | Command::AllNotes => "N",
            Command::Settings => ",",
            Command::Back => "[",
            Command::Forward => "]",
            Command::ToggleSidebar => "\\",
        }
    }

    pub fn shift(self) -> bool {
        self == Command::AllNotes
    }

    /// The shortcut as this platform writes it: "⌘N", "⇧⌘N" on Apple platforms,
    /// "Ctrl+N", "Ctrl+Shift+N" elsewhere.
    pub fn shortcut(self) -> String {
        shortcut_label(self.key(), self.shift(), APPLE)
    }
}

/// Whether shortcuts use Command (Apple platforms) rather than Control.
pub const APPLE: bool = cfg!(any(target_os = "macos", target_os = "ios"));

fn shortcut_label(key: &str, shift: bool, apple: bool) -> String {
    match (apple, shift) {
        (true, false) => format!("⌘{key}"),
        (true, true) => format!("⇧⌘{key}"),
        (false, false) => format!("Ctrl+{key}"),
        (false, true) => format!("Ctrl+Shift+{key}"),
    }
}

/// Run a command the way a key press would (see `MarginApp.command`).
#[cfg_attr(
    not(all(feature = "desktop", any(target_os = "macos", target_os = "linux"))),
    allow(dead_code)
)]
pub fn dispatch(command: Command) {
    let slug = serde_json::to_string(command.slug()).unwrap_or_default();
    let _ = crate::ui::common::js(&format!("MarginApp.command({slug});"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_round_trip() {
        for c in Command::ALL {
            assert_eq!(Command::from_slug(c.slug()), Some(c));
        }
        assert_eq!(Command::from_slug("nope"), None);
    }

    #[test]
    fn labels_follow_the_platform() {
        assert_eq!(shortcut_label("N", false, true), "⌘N");
        assert_eq!(shortcut_label("N", true, true), "⇧⌘N");
        assert_eq!(shortcut_label("K", false, false), "Ctrl+K");
        assert_eq!(shortcut_label("N", true, false), "Ctrl+Shift+N");
        assert!(Command::AllNotes.shift());
        assert_eq!(Command::Back.key(), "[");
    }
}
