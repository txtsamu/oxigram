//! Reactions under messages, the quick-reaction row of the message menu, and
//! setting or removing the user's own reaction.
//!
//! Paid (Telegram Stars) reactions are shown but never sent. Custom emoji
//! reactions are drawn as pictures when "Кастомные эмодзи в реакциях" is on,
//! otherwise counted together as one ✨.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use oxigram_core::store::chats::{MessageRow, Reaction};
use oxigram_core::{actions, chats, tr};
use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

use crate::{App, AppWindow, ReactionChip, ReactionRow, format, run, tg, with_app};

const CUSTOM: &str = "custom:";
const PAID: &str = "paid";
/// The usual reactions offered first in the menu. The heart is the plain
/// one: Telegram names reactions without U+FE0F.
const QUICK: [&str; 7] = ["👍", "❤", "🔥", "😂", "😮", "😢", "🙏"];
/// Room for chips is the widest bubble minus its padding.
const BUBBLE_PADDING: f32 = 24.0;
/// Gap between chips.
const CHIP_GAP: f32 = 4.0;
/// Drawn size of a custom emoji in a chip, logical pixels.
const EMOJI_PX: f32 = 18.0;

/// Pictures of custom emoji reactions, by document ID.
#[derive(Default)]
pub struct Emoji {
    pub(crate) images: HashMap<String, Image>,
    pending: HashSet<String>,
}

/// What the chat allows: unknown until asked, `None` = any emoji.
#[derive(Default)]
pub enum Allowed {
    #[default]
    Unknown,
    Any,
    Only(Vec<String>),
}

/// The reactions to draw: one entry per emoji, custom ones folded into a ✨
/// when their pictures are switched off.
fn shown(list: &[Reaction], custom_on: bool) -> Vec<Reaction> {
    if custom_on {
        return list.to_vec();
    }
    let is_custom = |r: &Reaction| r.emoji.is_empty() || r.emoji.starts_with(CUSTOM);
    let mut out: Vec<Reaction> = list.iter().filter(|r| !is_custom(r)).cloned().collect();
    let custom: Vec<&Reaction> = list.iter().filter(|r| is_custom(r)).collect();
    if !custom.is_empty() {
        out.push(Reaction {
            emoji: String::new(),
            count: custom.iter().map(|r| r.count).sum(),
            mine: custom.iter().any(|r| r.mine),
        });
    }
    out
}

impl App {
    /// The chip rows for the reactions of a bubble's parts.
    pub fn reaction_rows(&self, parts: &[&MessageRow]) -> ModelRc<ReactionRow> {
        let Some(target) = parts.iter().find(|p| !p.reactions.is_empty()) else {
            return ModelRc::default();
        };
        let chips: Vec<ReactionChip> = shown(&target.reactions, self.settings.custom_emoji)
            .into_iter()
            .map(|r| {
                let custom = r.emoji.strip_prefix(CUSTOM).map(str::to_owned);
                let image = custom.as_ref().and_then(|id| self.emoji.images.get(id)).cloned();
                let text = match r.emoji.as_str() {
                    PAID => "⭐".to_owned(),
                    "" => "✨".to_owned(),
                    _ if custom.is_some() => "✨".to_owned(),
                    emoji => emoji.to_owned(),
                };
                ReactionChip {
                    message: target.id,
                    // Paid ones cost money and the folded ✨ is no single
                    // reaction: neither can be clicked.
                    enabled: r.emoji != PAID && !r.emoji.is_empty(),
                    key: r.emoji.clone().into(),
                    custom: custom.unwrap_or_default().into(),
                    has_image: image.is_some(),
                    image: image.unwrap_or_default(),
                    text: text.into(),
                    count: r.count,
                    mine: r.mine,
                }
            })
            .collect();
        // Slint has no wrapping layout: rows are cut here, by estimated chip
        // widths, to the width of the widest bubble.
        let room = (self.reaction_width - BUBBLE_PADDING).max(120.0);
        let mut rows: Vec<Vec<ReactionChip>> = Vec::new();
        let mut used = 0.0;
        for chip in chips {
            let w = chip_width(&chip);
            match rows.last_mut() {
                Some(row) if used + CHIP_GAP + w <= room => {
                    used += CHIP_GAP + w;
                    row.push(chip);
                }
                _ => {
                    used = w;
                    rows.push(vec![chip]);
                }
            }
        }
        let rows: Vec<ReactionRow> = rows
            .into_iter()
            .map(|row| ReactionRow { chips: ModelRc::new(VecModel::from(row)) })
            .collect();
        ModelRc::new(VecModel::from(rows))
    }

    /// The quick reactions for the menu: the usual ones the chat allows first.
    fn quick(&self) -> Vec<String> {
        match &self.allowed {
            Allowed::Unknown | Allowed::Any => QUICK.iter().map(|s| (*s).to_owned()).collect(),
            Allowed::Only(list) => {
                let mut out: Vec<String> =
                    QUICK.iter().filter(|q| list.iter().any(|a| a == *q)).map(|s| (*s).to_owned()).collect();
                out.extend(list.iter().filter(|a| !QUICK.contains(&a.as_str())).cloned());
                out.truncate(QUICK.len());
                out
            }
        }
    }

    /// Applies new reactions to message `id` and redraws its bubble.
    pub fn set_reactions(&mut self, id: i32, reactions: Vec<Reaction>) {
        if let Some(index) = self.messages.iter().position(|m| m.id == id) {
            self.messages[index].reactions = reactions;
            self.refresh_row(index);
        }
    }
}

/// A chip's width as drawn: padding, the emoji, and the count's digits
/// (slightly generous, so a row never overflows).
fn chip_width(chip: &ReactionChip) -> f32 {
    let digits = if chip.count > 1 { chip.count.to_string().len() as f32 * 8.0 + 4.0 } else { 0.0 };
    17.0 + 21.0 + digits
}

pub fn wire(window: &AppWindow) {
    window.on_reaction_width(|width| {
        with_app(|app| {
            // Only a real change rewraps (and only bubbles with reactions).
            if (app.reaction_width - width).abs() < 4.0 {
                return;
            }
            app.reaction_width = width;
            let rows: Vec<usize> = app
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| !m.reactions.is_empty())
                .map(|(i, _)| i)
                .collect();
            for index in rows {
                app.refresh_row(index);
            }
        });
    });
    window.on_react(|message, key| {
        let key = key.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || react(message, &key));
    });
    window.on_menu_react(|emoji| {
        let target = with_app(|app| match app.menu_target.take() {
            Some(crate::context_menu::MenuTarget::Message(id)) => Some(id),
            _ => None,
        })
        .flatten();
        if let Some(id) = target {
            let emoji = emoji.to_string();
            slint::Timer::single_shot(Duration::ZERO, move || react(id, &emoji));
        }
    });
    window.on_want_emoji(|id| {
        let id = id.to_string();
        slint::Timer::single_shot(Duration::ZERO, move || request_emoji(id));
    });
}

/// Asks which reactions the open chat allows (for the menu's quick row).
pub fn load_allowed(peer: i64, generation: u64) {
    run(async move { actions::allowed_reactions(tg(), peer).await }, move |result| {
        with_app(|app| {
            if app.generation != generation {
                return;
            }
            app.allowed = match result {
                Ok(None) => Allowed::Any,
                Ok(Some(list)) => Allowed::Only(list),
                Err(_) => Allowed::Unknown,
            };
        });
    });
}

/// The quick-reaction row for a message's menu, with the own one marked.
pub fn fill_menu(ui: &AppWindow, id: i32) {
    let row = with_app(|app| {
        let m = app.messages.iter().find(|m| m.id == id)?;
        if m.kind == "service" {
            return None;
        }
        let mine = m.reactions.iter().find(|r| r.mine).map(|r| r.emoji.clone()).unwrap_or_default();
        Some((app.quick(), mine))
    })
    .flatten();
    let (quick, mine) = row.unwrap_or_default();
    let items: Vec<SharedString> = quick.into_iter().map(SharedString::from).collect();
    ui.set_menu_reactions(ModelRc::new(VecModel::from(items)));
    ui.set_menu_reaction_mine(mine.into());
}

/// Sets `key` as the own reaction, or removes it if it is already the own
/// one. Shown at once; put back if Telegram refuses.
fn react(id: i32, key: &str) {
    if key == PAID || key.is_empty() {
        return;
    }
    let key = key.replace('\u{fe0f}', "");
    let Some(Some((peer, old, next))) = with_app(|app| {
        let peer = app.chat?;
        let old = app.messages.iter().find(|m| m.id == id)?.reactions.clone();
        let next = if old.iter().any(|r| r.mine && r.emoji == key) { String::new() } else { key.clone() };
        let mut list: Vec<Reaction> = old
            .iter()
            .map(|r| if r.mine { Reaction { count: r.count - 1, mine: false, ..r.clone() } } else { r.clone() })
            .filter(|r| r.count > 0)
            .collect();
        if !next.is_empty() {
            match list.iter_mut().find(|r| r.emoji == next) {
                Some(same) => {
                    same.count += 1;
                    same.mine = true;
                }
                None => list.push(Reaction { emoji: next.clone(), count: 1, mine: true }),
            }
        }
        app.set_reactions(id, list);
        Some((peer, old, next))
    }) else {
        return;
    };
    run(async move { actions::react(tg(), peer, id, &next).await }, move |result| {
        if let Err(err) = result {
            with_app(|app| {
                if app.chat == Some(peer) {
                    app.set_reactions(id, old);
                }
            });
            crate::menus::toast(&error_text(&err));
        }
    });
}

fn error_text(err: &oxigram_core::error::UiError) -> String {
    match err.code.as_str() {
        "REACTION_INVALID" | "REACTION_EMPTY" => tr!("This reaction is not available in this chat."),
        "PREMIUM_ACCOUNT_REQUIRED" | "REACTIONS_TOO_MANY" => {
            tr!("This reaction needs Telegram Premium.")
        }
        "MESSAGE_NOT_MODIFIED" => tr!("The reaction is already set."),
        _ => tr!("The reaction was not set: {}", format::error(err)),
    }
}

/// Fetches a custom emoji picture, decoded to the chip's size.
pub(crate) fn request_emoji(id: String) {
    let Some(Some(px)) = with_app(|app| {
        // A forum topic icon is worth decoding even with message emoji off.
        let topic_icon = app
            .topics
            .iter()
            .any(|topic| topic.icon_emoji_id.as_deref() == Some(id.as_str()));
        if (!app.settings.custom_emoji && !topic_icon)
            || app.emoji.images.contains_key(&id)
            || app.emoji.pending.contains(&id)
        {
            return None;
        }
        app.emoji.pending.insert(id.clone());
        let scale = app.ui.upgrade().map_or(1.0, |ui| ui.window().scale_factor());
        Some((EMOJI_PX * scale).ceil() as u32)
    }) else {
        return;
    };
    let key = id.clone();
    run(
        async move {
            let path = chats::custom_emoji(tg(), &key).await.ok()?;
            tokio::task::spawn_blocking(move || {
                let picture = crate::pictures::open_image(path).ok()?;
                Some(picture.resize(px, px, image::imageops::FilterType::Triangle).to_rgba8())
            })
            .await
            .ok()
            .flatten()
        },
        move |pixels| {
            with_app(|app| {
                // A failure stays pending, so it is not retried in a loop.
                let Some(pixels) = pixels else { return };
                app.emoji.pending.remove(&id);
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    pixels.as_raw(),
                    pixels.width(),
                    pixels.height(),
                );
                app.emoji.images.insert(id.clone(), Image::from_rgba8(buffer));
                // A forum topic may use this emoji as its icon.
                if app
                    .topics
                    .iter()
                    .any(|topic| topic.icon_emoji_id.as_deref() == Some(id.as_str()))
                {
                    crate::apply_topics(app);
                }
                // Redraw every bubble that shows this emoji.
                let custom = format!("{CUSTOM}{id}");
                let rows: Vec<usize> = app
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.reactions.iter().any(|r| r.emoji == custom))
                    .map(|(i, _)| i)
                    .collect();
                for index in rows {
                    app.refresh_row(index);
                }
            });
        },
    );
}

pub fn drop_pictures(app: &mut App) {
    app.emoji = Emoji::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(emoji: &str, count: i32, mine: bool) -> Reaction {
        Reaction { emoji: emoji.into(), count, mine }
    }

    #[test]
    fn custom_reactions_fold_into_one_sparkle_when_off() {
        let list = [r("👍", 3, false), r("custom:1", 2, true), r("custom:2", 1, false), r("paid", 5, false)];
        let folded = shown(&list, false);
        assert_eq!(folded.len(), 3);
        assert_eq!(folded[2].emoji, "");
        assert_eq!(folded[2].count, 3);
        assert!(folded[2].mine);
        assert_eq!(shown(&list, true).len(), 4);
    }
}
