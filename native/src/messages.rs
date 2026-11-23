//! Message actions: reply, edit, copy, pin, forward and delete, plus sending
//! with the reply / edit state shown in the bar above the composer.

use oxigram_core::actions;
use oxigram_core::chats;
use oxigram_core::store::chats::{Entity, MessageRow};
use oxigram_core::{tr, trn};
use slint::{ModelRc, SharedString, VecModel};

use crate::{App, AppWindow, DialogItem, format, run, tg, ui, with_app};

/// What the next sent text does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Compose {
    None,
    /// Reply to this message.
    Reply(i32),
    /// Replace the text of this message.
    Edit(i32),
}

pub fn wire(ui: &AppWindow) {
    ui.on_bar_cancel(cancel_compose);
    ui.on_send_text(send_text);
    ui.on_forward_filter(|text| {
        with_app(|app| app.fill_forward(&text.to_lowercase()));
    });
    ui.on_forward_pick(forward_pick);
}

pub(crate) fn message_action(id: i32, name: SharedString) {
    let Some(Some(row)) = with_app(|app| app.messages.iter().find(|m| m.id == id).cloned()) else {
        return;
    };
    match name.as_str() {
        "reply" => start_reply(&row),
        "copy" => {
            if let Some(ui) = ui() {
                ui.invoke_copy_text(row.text.as_str().into());
            }
            crate::menus::toast(tr!("Text copied"));
        }
        // Makes this one bubble selectable (see MessageView in app.slint).
        "select" => {
            if let Some(ui) = ui() {
                ui.set_selecting_id(row.id);
            }
        }
        "edit" => start_edit(&row),
        "pin" => {
            let peer = row.peer_id;
            run(
                async move { actions::pin_message(tg(), peer, id, true).await },
                |done| match done {
                    Ok(()) => crate::menus::toast(tr!("Message pinned")),
                    Err(err) => crate::menus::toast(&format::error(&err)),
                },
            );
        }
        // An album goes as a whole.
        "forward" => {
            let ids = with_app(|app| app.with_albums(&[id])).unwrap_or_default();
            open_forward(ids);
        }
        "delete" => {
            let ids = with_app(|app| app.with_albums(&[row.id])).unwrap_or_default();
            ask_delete(ids, false);
        }
        _ => {}
    }
}

fn first_line(row: &MessageRow) -> String {
    match row.text.lines().find(|l| !l.trim().is_empty()) {
        Some(line) => line.to_owned(),
        None => format::kind_label(&row.kind).unwrap_or_default().to_owned(),
    }
}

fn set_bar(ui: &AppWindow, mode: &str, title: &str, text: &str) {
    ui.set_bar_mode(mode.into());
    ui.set_bar_title(title.into());
    ui.set_bar_text(text.into());
}

fn start_reply(row: &MessageRow) {
    with_app(|app| {
        app.compose = Compose::Reply(row.id);
        let who = if row.outgoing {
            tr!("You")
        } else if row.sender_name.is_empty() {
            app.chat
                .and_then(|peer| app.dialog(peer))
                .map(format::dialog_title)
                .unwrap_or_default()
        } else {
            row.sender_name.clone()
        };
        if let Some(ui) = app.ui.upgrade() {
            set_bar(&ui, "reply", &tr!("Reply to {}", who), &first_line(row));
            ui.invoke_focus_composer();
        }
    });
}

pub(crate) fn start_edit(row: &MessageRow) {
    if !(row.outgoing && row.kind == "text") {
        return;
    }
    with_app(|app| {
        app.compose = Compose::Edit(row.id);
        if let Some(ui) = app.ui.upgrade() {
            set_bar(&ui, "edit", &tr!("Editing"), &first_line(row));
            ui.invoke_set_composer(to_markup(&row.text, &row.entities).into());
            ui.invoke_focus_composer();
        }
    });
}

/// The bar's cross. The composer has no getter for the text typed before an
/// edit began, so cancelling an edit clears it instead of restoring a draft.
fn cancel_compose() {
    with_app(|app| {
        let was_edit = matches!(app.compose, Compose::Edit(_));
        app.compose = Compose::None;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_bar_mode(SharedString::new());
            if was_edit {
                // The draft from before the edit comes back.
                ui.invoke_set_composer(app.chat.map(crate::drafts::get).unwrap_or_default().into());
            }
        }
    });
}

/// Opens the forward picker for these messages.
pub(crate) fn open_forward(ids: Vec<i32>) {
    with_app(|app| {
        app.forward_ids = ids;
        app.fill_forward("");
        if let Some(ui) = app.ui.upgrade() {
            ui.set_forward_open(true);
        }
    });
}

/// Asks, then deletes these messages (one bubble, an album or a selection).
/// `selection`: leave selection mode once done.
pub(crate) fn ask_delete(ids: Vec<i32>, selection: bool) {
    let Some(Some((peer, kind, all_own, bubbles))) = with_app(|app| {
        let peer = app.chat?;
        let all_own = ids.iter().all(|id| app.messages.iter().find(|m| m.id == *id).is_some_and(|m| m.outgoing));
        let bubbles = if selection { app.selected.len() } else { 1 };
        Some((peer, app.chat_kind().to_owned(), all_own, bubbles))
    }) else {
        return;
    };
    let big = peer <= -1_000_000_000_000;
    let check = if !big && all_own && kind != "saved" {
        if kind == "user" { tr!("Also delete for the other person") } else { tr!("Also delete for everyone") }
    } else {
        String::new()
    };
    let has_check = !check.is_empty();
    let title = if bubbles > 1 {
        trn!("Delete {n} message?", "Delete {n} messages?", bubbles)
    } else {
        tr!("Delete message?")
    };
    crate::menus::confirm(
        &title,
        &tr!("This action cannot be undone."),
        &tr!("Delete"),
        &check,
        move |checked| {
            let for_everyone = big || (has_check && checked);
            // Gone from the chat at once, as in the official apps: the request
            // can take a while (a connection that slept reconnects first).
            with_app(|app| {
                for id in &ids {
                    app.remove_message(peer, *id);
                }
                if selection {
                    app.clear_selection();
                }
            });
            run(
                async move { actions::delete_messages(tg(), peer, &ids, for_everyone).await },
                move |done| {
                    if let Err(err) = done {
                        crate::menus::toast(tr!("Could not delete: {}", format::error(&err)));
                        // Bring the messages back by reloading the chat.
                        if with_app(|app| app.chat == Some(peer)).unwrap_or(false) {
                            crate::open_chat(peer);
                        }
                    }
                },
            );
        },
    );
}

fn forward_pick(index: i32) {
    let Some(Some((from, to, ids))) = with_app(|app| {
        let to = app.forward_shown.get(usize::try_from(index).ok()?).copied()?;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_forward_open(false);
        }
        Some((app.chat?, to, std::mem::take(&mut app.forward_ids)))
    }) else {
        return;
    };
    run(
        async move { actions::forward(tg(), from, &ids, to).await },
        move |done| match done {
            Ok(rows) => {
                // Forwarding into the open chat shows the copies at once.
                with_app(|app| {
                    for row in rows {
                        app.add_message(row);
                    }
                });
                crate::menus::toast(tr!("Forwarded"));
            }
            Err(err) => crate::menus::toast(&format::error(&err)),
        },
    );
}

fn send_text(text: SharedString) {
    let text = text.to_string();
    if text.trim().is_empty() {
        return;
    }
    let Some(Some((peer, compose, topic))) = with_app(|app| app.chat.map(|peer| (peer, app.compose, app.topic))) else {
        return;
    };
    if let Some(ui) = ui() {
        // After an edit the draft from before it comes back.
        let rest = if matches!(compose, Compose::Edit(_)) { crate::drafts::get(peer) } else { String::new() };
        ui.invoke_set_composer(rest.into());
        ui.set_chat_error(SharedString::new());
        ui.set_bar_mode(SharedString::new());
        ui.invoke_scroll_to_bottom();
    }
    with_app(|app| app.compose = Compose::None);
    let draft = text.clone();
    let failed = move |app: &mut App, err: &oxigram_core::error::UiError| {
        app.chat_error(err);
        if app.chat == Some(peer) {
            if let Some(ui) = app.ui.upgrade() {
                ui.invoke_set_composer(draft.into());
            }
        } else {
            // Another chat is open now: the text goes back to its own draft.
            crate::drafts::set(peer, &draft);
        }
    };
    match compose {
        Compose::Edit(id) => {
            let edited = text.trim().to_owned();
            run(
                async move { actions::edit_message(tg(), peer, id, &text).await },
                move |done| {
                    with_app(|app| match done {
                        Ok(()) => app.set_edited(peer, id, edited),
                        Err(err) => failed(app, &err),
                    });
                },
            );
        }
        Compose::Reply(_) | Compose::None => {
            let reply_to = match compose {
                Compose::Reply(id) => Some(id),
                _ => None,
            };
            run(
                async move { chats::send_message(tg(), peer, &text, reply_to, topic).await },
                move |sent| {
                    with_app(|app| match sent {
                        Ok(row) => app.add_message(row),
                        Err(err) => failed(app, &err),
                    });
                },
            );
        }
    }
}

impl App {
    /// Fills the forward picker: active chats first, then the archive, narrowed
    /// by `needle` (lowercase part of a title).
    pub(crate) fn fill_forward(&mut self, needle: &str) {
        let rows: Vec<&oxigram_core::store::chats::DialogRow> = self
            .dialogs
            .iter()
            .filter(|d| !d.archived)
            .chain(self.dialogs.iter().filter(|d| d.archived))
            .filter(|d| needle.is_empty() || format::dialog_title(d).to_lowercase().contains(needle))
            .collect();
        let shown: Vec<i64> = rows.iter().map(|d| d.id).collect();
        let items: Vec<DialogItem> = rows.into_iter().map(|d| self.dialog_item(d)).collect();
        self.forward_shown = shown;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_forward_items(ModelRc::from(std::rc::Rc::new(VecModel::from(items))));
        }
    }

    /// Takes a message out of the open chat, like a live `Deleted` event.
    fn remove_message(&mut self, peer: i64, id: i32) {
        if self.chat != Some(peer) {
            return;
        }
        if self.compose == Compose::Reply(id) || self.compose == Compose::Edit(id) {
            let was_edit = matches!(self.compose, Compose::Edit(_));
            self.compose = Compose::None;
            if let Some(ui) = self.ui.upgrade() {
                ui.set_bar_mode(SharedString::new());
                if was_edit {
                    ui.invoke_set_composer(crate::drafts::get(peer).into());
                }
            }
        }
        self.remove_ids(&[id]);
    }

    /// Shows the text of an edit that Telegram accepted; the live `Edited`
    /// update that follows brings the real formatting.
    fn set_edited(&mut self, peer: i64, id: i32, text: String) {
        if self.chat != Some(peer) {
            return;
        }
        if let Some(index) = self.messages.iter().position(|m| m.id == id) {
            let m = &mut self.messages[index];
            m.text = text;
            m.edited = true;
            self.refresh_row(index);
        }
    }
}

/// Inverse of the formatting parser: puts the markup marks back around the
/// formatted spans so an edit keeps them. Entity offsets count UTF-16 units.
pub(crate) fn to_markup(text: &str, entities: &[Entity]) -> String {
    fn mark(kind: &str) -> Option<&'static str> {
        Some(match kind {
            "bold" => "**",
            "italic" => "__",
            "strike" => "~~",
            "spoiler" => "||",
            "code" => "`",
            "pre" => "```",
            _ => return None,
        })
    }
    // Byte index of every UTF-16 unit; units inside a char map to its end.
    let mut at_unit: Vec<usize> = Vec::with_capacity(text.len() + 1);
    for (byte, ch) in text.char_indices() {
        at_unit.push(byte);
        if ch.len_utf16() == 2 {
            at_unit.push(byte + ch.len_utf8());
        }
    }
    at_unit.push(text.len());
    let byte_at = |unit: i32| at_unit[usize::try_from(unit).unwrap_or(0).min(at_unit.len() - 1)];

    let mut inserts: Vec<(usize, &str, bool)> = Vec::new();
    for e in entities {
        let Some(mark) = mark(&e.kind) else { continue };
        inserts.push((byte_at(e.offset), mark, true));
        inserts.push((byte_at(e.offset.saturating_add(e.length)), mark, false));
    }
    // Back to front, so earlier positions stay valid; at one position the
    // opening mark is applied first and so ends up after the closing one.
    inserts.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.cmp(&a.2)));
    let mut out = text.to_owned();
    for (at, mark, _) in inserts {
        out.insert_str(at, mark);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(kind: &str, offset: i32, length: i32) -> Entity {
        Entity { kind: kind.into(), offset, length, target: None }
    }

    #[test]
    fn marks_cyrillic_span() {
        assert_eq!(to_markup("привет мир", &[entity("bold", 0, 6)]), "**привет** мир");
    }

    #[test]
    fn offsets_are_utf16_after_an_emoji() {
        // The emoji is two UTF-16 units but four bytes.
        assert_eq!(to_markup("😀 мир", &[entity("italic", 3, 3)]), "😀 __мир__");
    }

    #[test]
    fn nested_and_adjacent_spans() {
        let e = [entity("bold", 0, 3), entity("code", 0, 3), entity("strike", 3, 2)];
        assert_eq!(to_markup("abcde", &e), "`**abc`**~~de~~");
    }

    #[test]
    fn unknown_kinds_and_overflow_are_safe() {
        assert_eq!(to_markup("abc", &[entity("url", 0, 3)]), "abc");
        assert_eq!(to_markup("abc", &[entity("bold", 1, 50)]), "a**bc**");
    }
}
