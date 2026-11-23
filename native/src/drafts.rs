//! Drafts: text typed in the composer and left unsent stays with its chat.
//! As in the WebView build they live on this computer only (never sent to
//! Telegram) and survive a restart: `native-drafts.json` next to the settings.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use crate::messages::Compose;
use crate::{AppWindow, with_app};

const FILE: &str = "native-drafts.json";
/// Typing is written to disk this long after the last keystroke.
const SAVE_AFTER: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Drafts {
    by_peer: HashMap<i64, String>,
    save_pending: bool,
}

thread_local! {
    static DRAFTS: RefCell<Drafts> = RefCell::new(Drafts::default());
}

/// Reads the saved drafts; call once at start-up (after the folder is set).
pub fn load() {
    let Some(dir) = crate::settings::dir() else { return };
    let map: HashMap<i64, String> = std::fs::read_to_string(dir.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<HashMap<String, String>>(&text).ok())
        .map(|raw| {
            raw.into_iter()
                .filter_map(|(peer, text)| Some((peer.parse().ok()?, text)))
                .filter(|(_, text)| !text.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    DRAFTS.with(|drafts| drafts.borrow_mut().by_peer = map);
}

/// The draft of a chat, empty when there is none.
pub fn get(peer: i64) -> String {
    DRAFTS.with(|drafts| drafts.borrow().by_peer.get(&peer).cloned().unwrap_or_default())
}

/// The draft as the chat list's one-line preview.
pub fn preview(peer: i64) -> Option<String> {
    DRAFTS.with(|drafts| {
        let drafts = drafts.borrow();
        let text = drafts.by_peer.get(&peer)?;
        Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

/// Remembers (or, for blank text, drops) a draft. True when it changed.
pub fn set(peer: i64, text: &str) -> bool {
    let changed = DRAFTS.with(|drafts| {
        let mut drafts = drafts.borrow_mut();
        if text.trim().is_empty() {
            drafts.by_peer.remove(&peer).is_some()
        } else if drafts.by_peer.get(&peer).is_some_and(|old| old == text) {
            false
        } else {
            drafts.by_peer.insert(peer, text.to_owned());
            true
        }
    });
    if changed {
        save_soon();
    }
    changed
}

/// Forgets every draft (sign-out).
pub fn clear() {
    DRAFTS.with(|drafts| drafts.borrow_mut().by_peer.clear());
    write_now();
}

fn save_soon() {
    if crate::settings::dir().is_none() {
        return;
    }
    let start = DRAFTS.with(|drafts| {
        let mut drafts = drafts.borrow_mut();
        !std::mem::replace(&mut drafts.save_pending, true)
    });
    if start {
        slint::Timer::single_shot(SAVE_AFTER, write_now);
    }
}

/// A passcode was set: the readable drafts file goes; drafts stay in memory
/// until the app locks or quits.
pub fn forget_file() {
    if let Some(dir) = crate::settings::dir() {
        oxigram_core::vault::shred(&dir.join(FILE));
    }
}

/// The passcode was turned off: drafts are kept on disk again.
pub fn save_now() {
    write_now();
}

/// On lock: drafts are decrypted data too.
pub fn forget_all() {
    DRAFTS.with(|drafts| drafts.borrow_mut().by_peer.clear());
}

fn write_now() {
    let Some(dir) = crate::settings::dir() else { return };
    if crate::tg().passcode_on() {
        return;
    }
    let text = DRAFTS.with(|drafts| {
        let mut drafts = drafts.borrow_mut();
        drafts.save_pending = false;
        let raw: HashMap<String, &String> = drafts.by_peer.iter().map(|(peer, text)| (peer.to_string(), text)).collect();
        serde_json::to_string(&raw).ok()
    });
    if let Some(text) = text {
        let _ = std::fs::write(dir.join(FILE), text);
    }
}

pub fn wire(window: &AppWindow) {
    // Every change of the composer lands here, typed or set by the program.
    window.on_composer_changed(|text| {
        // An edit shows the message's own text in the field: not a draft.
        let Some((Some(peer), editing)) = with_app(|app| (app.chat, matches!(app.compose, Compose::Edit(_)))) else {
            return;
        };
        if !editing {
            set(peer, text.as_str());
        }
        let commands = with_app(|app| app.bot_commands.clone()).unwrap_or_default();
        crate::commands::suggest(&commands, text.as_str());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_text_is_no_draft() {
        assert!(set(5, "привет"));
        assert!(!set(5, "привет"));
        assert_eq!(get(5), "привет");
        assert!(set(5, "  \n"));
        assert_eq!(get(5), "");
        assert!(!set(5, ""));
    }

    #[test]
    fn preview_is_one_line() {
        set(6, "раз\n два   три");
        assert_eq!(preview(6).as_deref(), Some("раз два три"));
        set(6, "");
        assert_eq!(preview(6), None);
    }
}
