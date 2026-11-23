//! Local cache of the dialog list and message history.
//!
//! The cache exists so a chat opens instantly; Telegram stays the source of
//! truth. Whatever the server no longer returns is removed here as well.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// One row of the chat list. `id` is the Bot-API style dialog ID, which is
/// unique across users, groups and channels.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DialogRow {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub last_text: String,
    pub last_date: i64,
    pub last_outgoing: bool,
    /// ID of the last message, to tell whether an own one was read.
    pub last_id: i32,
    pub unread: i32,
    pub pinned: bool,
    /// Lives in the Archive folder rather than the main list.
    pub archived: bool,
    /// Profile photo ID, as a string: it does not fit a JavaScript number.
    pub photo_id: Option<String>,
    /// Own messages up to this ID have been read by the other side.
    pub read_outbox: i32,
    /// Reactions to own messages not seen yet.
    pub unread_reactions: i32,
    /// Notifications are off for this chat.
    pub muted: bool,
    /// A private chat with someone in the user's contacts.
    pub contact: bool,
    /// A private chat with a bot.
    pub bot: bool,
    /// Online status of a private chat's user. Changes too often to be worth
    /// caching, so rows loaded from disk have none.
    pub presence: Option<Presence>,
    /// The user may write here (a channel only for its creator and admins
    /// who can post). Not cached: a column would break older builds sharing
    /// the database, so rows from disk assume channels are read-only until
    /// the list arrives from Telegram.
    pub can_post: bool,
}

/// What Telegram reveals about when a user was last online.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    /// "online", "offline", "recently", "week", "month" or "hidden".
    pub kind: &'static str,
    /// Unix time: until when "online" holds, or when "offline" began.
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRow {
    pub id: i32,
    pub peer_id: i64,
    pub sender_id: Option<i64>,
    pub sender_name: String,
    pub date: i64,
    pub text: String,
    /// "text", or what stands in for content the client does not render yet.
    pub kind: String,
    pub outgoing: bool,
    pub edited: bool,
    /// Size of the picture `message_image` can fetch; 0 when there is none.
    pub image_w: i32,
    pub image_h: i32,
    pub file_name: String,
    pub file_size: i64,
    /// The full file can be fetched with `message_file` and played or shown.
    pub playable: bool,
    /// Seconds of audio or video; 0 when not known.
    pub duration: i32,
    /// Voice messages: the waveform, 5 bits per bar, packed (see Telegram's
    /// `DocumentAttributeAudio.waveform`). The Tauri front end ignores it.
    /// Not cached: the database is shared with installed Tauri builds, whose
    /// positional INSERT breaks on extra columns; fresh history brings it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waveform: Vec<u8>,
    /// Audio files: "performer — title" when the tags carry them (not cached
    /// either, see `waveform`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub track: String,
    /// Messages sharing this value form one album.
    pub grouped_id: Option<String>,
    pub reactions: Vec<Reaction>,
    /// The message this one answers, in the same chat.
    pub reply_to_id: Option<i32>,
    /// The forum topic the message belongs to. Not cached, like `waveform`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic_id: Option<i32>,
    /// A channel post's comments: how many there are, 0 when it has no
    /// discussion group.
    pub comments: i32,
    /// Formatting and links inside `text` (offsets in UTF-16 units, as in
    /// JavaScript strings).
    pub entities: Vec<Entity>,
    /// The link preview under the text.
    pub web: Option<WebPreview>,
    /// Inline buttons under the message (bots, channels), row by row. Not
    /// cached either (see `waveform`); fresh history brings them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<Vec<InlineButton>>,
}

/// An inline button.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineButton {
    pub text: String,
    /// "url", "callback" (value: the data in hex), "copy" (value: the
    /// text), "user" (value: the user's ID), or "other" (value empty: works
    /// only in Telegram's own apps).
    pub kind: String,
    pub value: String,
}

/// One span of formatting or a link inside a message text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entity {
    /// "bold", "italic", "underline", "strike", "code", "pre", "spoiler",
    /// "quote", "url", "textUrl", "mention", "mentionName", "hashtag",
    /// "email", "phone", "command".
    pub kind: String,
    pub offset: i32,
    pub length: i32,
    /// Target of a "textUrl", or the user ID of a "mentionName".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebPreview {
    pub url: String,
    pub site: String,
    pub title: String,
    pub description: String,
}

/// One reaction under a message, as Telegram counts it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reaction {
    /// The emoji, or "" for a custom (premium) emoji the client cannot draw.
    pub emoji: String,
    pub count: i32,
    /// The user's own reaction.
    pub mine: bool,
}

/// Both tables are caches, so a layout change simply recreates them and the
/// rows are fetched again.
pub const SCHEMA: &str = "
    DROP TABLE IF EXISTS dialogs;
    DROP TABLE IF EXISTS messages;
    CREATE TABLE dialogs (
        position INTEGER PRIMARY KEY,
        peer_id INTEGER NOT NULL UNIQUE,
        kind TEXT NOT NULL,
        title TEXT NOT NULL,
        last_text TEXT NOT NULL,
        last_date INTEGER NOT NULL,
        last_outgoing INTEGER NOT NULL,
        unread INTEGER NOT NULL,
        pinned INTEGER NOT NULL,
        archived INTEGER NOT NULL,
        photo_id TEXT,
        read_outbox INTEGER NOT NULL,
        unread_reactions INTEGER NOT NULL,
        last_id INTEGER NOT NULL,
        muted INTEGER NOT NULL,
        contact INTEGER NOT NULL,
        bot INTEGER NOT NULL);
    CREATE TABLE messages (
        peer_id INTEGER NOT NULL,
        id INTEGER NOT NULL,
        sender_id INTEGER,
        sender_name TEXT NOT NULL,
        date INTEGER NOT NULL,
        text TEXT NOT NULL,
        kind TEXT NOT NULL,
        outgoing INTEGER NOT NULL,
        edited INTEGER NOT NULL,
        image_w INTEGER NOT NULL,
        image_h INTEGER NOT NULL,
        file_name TEXT NOT NULL,
        file_size INTEGER NOT NULL,
        playable INTEGER NOT NULL,
        grouped_id TEXT,
        reactions TEXT NOT NULL,
        reply_to_id INTEGER,
        entities TEXT NOT NULL,
        web TEXT,
        duration INTEGER NOT NULL,
        comments INTEGER NOT NULL,
        PRIMARY KEY (peer_id, id)) WITHOUT ROWID;";

/// Stores one page of the dialog list. `start` is the position of the first
/// row; a page starting at 0 replaces the whole cached list.
pub fn save_dialogs(conn: &mut Connection, start: usize, rows: &[DialogRow]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    if start == 0 {
        tx.execute("DELETE FROM dialogs", [])?;
    }
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO dialogs VALUES (?1, ?2, ?3, enc(?4), enc(?5), ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )?;
        for (offset, row) in rows.iter().enumerate() {
            // A dialog that moved between pages must not linger at its old position.
            tx.execute("DELETE FROM dialogs WHERE peer_id = ?1", [row.id])?;
            stmt.execute(params![
                (start + offset) as i64,
                row.id,
                row.kind,
                row.title,
                row.last_text,
                row.last_date,
                row.last_outgoing,
                row.unread,
                row.pinned,
                row.archived,
                row.photo_id,
                row.read_outbox,
                row.unread_reactions,
                row.last_id,
                row.muted,
                row.contact,
                row.bot,
            ])?;
        }
    }
    tx.commit()
}

/// Drops a chat's cached messages and its row in the list (after it was
/// cleared or deleted).
pub fn forget_chat(conn: &mut Connection, peer_id: i64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM messages WHERE peer_id = ?1", [peer_id])?;
    tx.execute("DELETE FROM dialogs WHERE peer_id = ?1", [peer_id])?;
    tx.commit()
}

/// Replaces the reactions shown under one message.
pub fn set_reactions(
    conn: &Connection,
    peer_id: i64,
    id: i32,
    reactions: &[Reaction],
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE messages SET reactions = ?3 WHERE peer_id = ?1 AND id = ?2",
        params![peer_id, id, encode_reactions(reactions)],
    )?;
    Ok(())
}

fn to_json<T: Serialize>(items: &[T]) -> String {
    if items.is_empty() {
        return "[]".to_owned();
    }
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_owned())
}

fn encode_reactions(reactions: &[Reaction]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    serde_json::to_string(reactions).unwrap_or_default()
}

fn decode_reactions(text: &str) -> Vec<Reaction> {
    if text.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(text).unwrap_or_default()
}

/// Whether notifications are off for a chat; `None` when the chat is not
/// in the cached list.
pub fn is_muted(conn: &Connection, peer_id: i64) -> Option<bool> {
    conn.query_row("SELECT muted FROM dialogs WHERE peer_id = ?1", [peer_id], |row| row.get(0))
        .ok()
}

/// The chat's name as the list shows it, when the chat is in the list.
pub fn dialog_title(conn: &Connection, peer_id: i64) -> Option<String> {
    conn.query_row("SELECT dec(title) FROM dialogs WHERE peer_id = ?1", [peer_id], |row| row.get(0))
        .ok()
        .filter(|title: &String| !title.is_empty())
}

/// The profile photo ID of a listed chat.
pub fn dialog_photo(conn: &Connection, peer_id: i64) -> Option<String> {
    conn.query_row("SELECT photo_id FROM dialogs WHERE peer_id = ?1", [peer_id], |row| row.get::<_, Option<String>>(0))
        .ok()
        .flatten()
        .filter(|photo| !photo.is_empty())
}

pub fn set_muted(conn: &Connection, peer_id: i64, muted: bool) -> rusqlite::Result<()> {
    conn.execute("UPDATE dialogs SET muted = ?2 WHERE peer_id = ?1", params![peer_id, muted])?;
    Ok(())
}

/// Records that the other side read own messages up to `max_id`.
pub fn set_read_outbox(conn: &Connection, peer_id: i64, max_id: i32) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE dialogs SET read_outbox = MAX(read_outbox, ?2) WHERE peer_id = ?1",
        params![peer_id, max_id],
    )?;
    Ok(())
}

pub fn load_dialogs(conn: &Connection) -> rusqlite::Result<Vec<DialogRow>> {
    let mut stmt = conn.prepare(
        "SELECT peer_id, kind, dec(title), dec(last_text), last_date, last_outgoing, unread, pinned, archived, photo_id,
                read_outbox, unread_reactions, last_id, muted, contact, bot
         FROM dialogs ORDER BY position",
    )?;
    stmt.query_map([], |row| {
        Ok(DialogRow {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            last_text: row.get(3)?,
            last_date: row.get(4)?,
            last_outgoing: row.get(5)?,
            unread: row.get(6)?,
            pinned: row.get(7)?,
            archived: row.get(8)?,
            photo_id: row.get(9)?,
            read_outbox: row.get(10)?,
            unread_reactions: row.get(11)?,
            last_id: row.get(12)?,
            muted: row.get(13)?,
            contact: row.get(14)?,
            bot: row.get(15)?,
            presence: None,
            can_post: true,
        })
    })?
    .map(|row| {
        row.map(|mut row| {
            row.can_post = row.kind != "channel";
            row
        })
    })
    .collect()
}

/// The number of unread messages after a read on any device.
pub fn set_unread(conn: &Connection, peer_id: i64, unread: i32) -> rusqlite::Result<()> {
    conn.execute("UPDATE dialogs SET unread = ?2 WHERE peer_id = ?1", params![peer_id, unread])?;
    Ok(())
}

/// A manual "unread" mark shows as a count of at least 1; taking it off
/// clears only that mark (a real count stays).
pub fn set_unread_mark(conn: &Connection, peer_id: i64, unread: bool) -> rusqlite::Result<()> {
    let sql = if unread {
        "UPDATE dialogs SET unread = MAX(unread, 1) WHERE peer_id = ?1"
    } else {
        "UPDATE dialogs SET unread = 0 WHERE peer_id = ?1 AND unread = 1"
    };
    conn.execute(sql, [peer_id])?;
    Ok(())
}

pub fn clear_unread(conn: &Connection, peer_id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE dialogs SET unread = 0 WHERE peer_id = ?1", [peer_id])?;
    Ok(())
}

pub fn save_messages(conn: &mut Connection, rows: &[MessageRow]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO messages
             VALUES (?1, ?2, ?3, enc(?4), ?5, enc(?6), ?7, ?8, ?9, ?10, ?11, enc(?12), ?13, ?14, ?15, ?16, ?17, enc(?18), enc(?19), ?20, ?21)",
        )?;
        for row in rows {
            stmt.execute(params![
                row.peer_id,
                row.id,
                row.sender_id,
                row.sender_name,
                row.date,
                row.text,
                row.kind,
                row.outgoing,
                row.edited,
                row.image_w,
                row.image_h,
                row.file_name,
                row.file_size,
                row.playable,
                row.grouped_id,
                encode_reactions(&row.reactions),
                row.reply_to_id,
                to_json(&row.entities),
                row.web.as_ref().map(|web| serde_json::to_string(web).unwrap_or_default()),
                row.duration,
                row.comments,
            ])?;
        }
    }
    tx.commit()
}

/// Replaces the cached slice `(.., before_id)` of a chat with a page freshly
/// fetched from Telegram (`rows`, newest first, at most `limit` of them).
///
/// Cached messages inside the span the page covers that the server did not
/// return were deleted remotely, so they are deleted here too.
pub fn sync_history_page(
    conn: &mut Connection,
    peer_id: i64,
    before_id: Option<i32>,
    limit: usize,
    rows: &[MessageRow],
) -> rusqlite::Result<()> {
    let upper = before_id.unwrap_or(i32::MAX);
    // A short page means the server has nothing older: the span reaches the
    // start of the chat. A full page only vouches for IDs down to its oldest.
    let lower = if rows.len() < limit {
        0
    } else {
        rows.iter().map(|row| row.id).min().unwrap_or(0)
    };
    conn.execute(
        "DELETE FROM messages WHERE peer_id = ?1 AND id >= ?2 AND id < ?3",
        params![peer_id, lower, upper],
    )?;
    save_messages(conn, rows)
}

/// Smallest dialog ID that is not a channel or supergroup. Messages in those
/// have per-chat IDs; everywhere else IDs are unique across the account.
const CHANNEL_ID_CEILING: i64 = -1_000_000_000_000;

/// Removes messages Telegram reported as deleted. Deletions outside channels
/// arrive without a chat, only with account-wide message IDs.
pub fn delete_messages(
    conn: &mut Connection,
    channel_peer_id: Option<i64>,
    ids: &[i32],
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut in_channel =
            tx.prepare("DELETE FROM messages WHERE peer_id = ?1 AND id = ?2")?;
        let mut elsewhere =
            tx.prepare("DELETE FROM messages WHERE id = ?1 AND peer_id > ?2")?;
        for id in ids {
            match channel_peer_id {
                Some(peer_id) => in_channel.execute(params![peer_id, id])?,
                None => elsewhere.execute(params![id, CHANNEL_ID_CEILING])?,
            };
        }
    }
    tx.commit()
}

/// Newest first, like the network API.
pub fn load_messages(
    conn: &Connection,
    peer_id: i64,
    before_id: Option<i32>,
    limit: usize,
) -> rusqlite::Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, peer_id, sender_id, dec(sender_name), date, dec(text), kind, outgoing, edited,
                image_w, image_h, dec(file_name), file_size, playable, grouped_id, reactions, reply_to_id, dec(entities), dec(web), duration, comments
         FROM messages WHERE peer_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    stmt.query_map(
        params![peer_id, before_id.unwrap_or(i32::MAX), limit as i64],
        |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                peer_id: row.get(1)?,
                sender_id: row.get(2)?,
                sender_name: row.get(3)?,
                date: row.get(4)?,
                text: row.get(5)?,
                kind: row.get(6)?,
                outgoing: row.get(7)?,
                edited: row.get(8)?,
                image_w: row.get(9)?,
                image_h: row.get(10)?,
                file_name: row.get(11)?,
                file_size: row.get(12)?,
                playable: row.get(13)?,
                grouped_id: row.get(14)?,
                reactions: decode_reactions(&row.get::<_, String>(15)?),
                reply_to_id: row.get(16)?,
                topic_id: None,
                entities: serde_json::from_str(&row.get::<_, String>(17)?).unwrap_or_default(),
                web: row
                    .get::<_, Option<String>>(18)?
                    .and_then(|text| serde_json::from_str(&text).ok()),
                duration: row.get(19)?,
                comments: row.get(20)?,
                waveform: Vec::new(),
                track: String::new(),
                buttons: Vec::new(),
            })
        },
    )?
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn dialog(id: i64, title: &str) -> DialogRow {
        DialogRow {
            id,
            kind: "user".into(),
            title: title.into(),
            last_text: "hi".into(),
            last_date: 100,
            last_outgoing: false,
            last_id: 0,
            unread: 3,
            pinned: false,
            archived: false,
            photo_id: None,
            read_outbox: 0,
            unread_reactions: 0,
            muted: false,
            contact: false,
            bot: false,
            presence: None,
            can_post: true,
        }
    }

    fn message(id: i32) -> MessageRow {
        MessageRow {
            id,
            peer_id: 7,
            sender_id: Some(7),
            sender_name: "Ann".into(),
            date: 1000 + i64::from(id),
            text: format!("message {id}"),
            kind: "text".into(),
            outgoing: id % 2 == 0,
            edited: false,
            image_w: 0,
            image_h: 0,
            file_name: String::new(),
            file_size: 0,
            playable: false,
            grouped_id: None,
            reactions: Vec::new(),
            reply_to_id: None,
            topic_id: None,
            comments: 0,
            entities: Vec::new(),
            web: None,
            duration: 0,
            waveform: Vec::new(),
            track: String::new(),
            buttons: Vec::new(),
        }
    }

    fn ids(rows: &[MessageRow]) -> Vec<i32> {
        rows.iter().map(|row| row.id).collect()
    }

    #[test]
    fn dialog_pages_keep_server_order() {
        let db = store::open(":memory:").unwrap();
        let mut conn = db.lock().unwrap();

        save_dialogs(&mut conn, 0, &[dialog(1, "a"), dialog(2, "b")]).unwrap();
        save_dialogs(&mut conn, 2, &[dialog(3, "c")]).unwrap();
        let titles = |conn: &Connection| -> Vec<String> {
            load_dialogs(conn).unwrap().into_iter().map(|d| d.title).collect()
        };
        assert_eq!(titles(&conn), ["a", "b", "c"]);

        // A dialog seen again on a later page moves instead of duplicating.
        save_dialogs(&mut conn, 3, &[dialog(1, "a")]).unwrap();
        assert_eq!(titles(&conn), ["b", "c", "a"]);

        clear_unread(&conn, 2).unwrap();
        assert_eq!(load_dialogs(&conn).unwrap()[0].unread, 0);

        // A first page replaces everything.
        save_dialogs(&mut conn, 0, &[dialog(9, "z")]).unwrap();
        assert_eq!(titles(&conn), ["z"]);
    }

    #[test]
    fn history_is_paged_newest_first() {
        let db = store::open(":memory:").unwrap();
        let mut conn = db.lock().unwrap();
        let rows: Vec<_> = (1..=10).map(message).collect();
        save_messages(&mut conn, &rows).unwrap();

        let page = load_messages(&conn, 7, None, 4).unwrap();
        assert_eq!(ids(&page), [10, 9, 8, 7]);
        assert_eq!(page[0], message(10));
        assert_eq!(ids(&load_messages(&conn, 7, Some(7), 4).unwrap()), [6, 5, 4, 3]);
        assert!(load_messages(&conn, 8, None, 4).unwrap().is_empty());
    }

    #[test]
    fn remote_deletions_respect_the_two_id_spaces() {
        let db = store::open(":memory:").unwrap();
        let mut conn = db.lock().unwrap();
        let channel = -1_000_000_000_123;
        let in_chat = |peer_id: i64, id: i32| MessageRow { peer_id, ..message(id) };
        save_messages(
            &mut conn,
            &[in_chat(7, 5), in_chat(8, 6), in_chat(channel, 5), in_chat(channel, 6)],
        )
        .unwrap();

        // No chat given: only non-channel messages with that ID go.
        delete_messages(&mut conn, None, &[5]).unwrap();
        assert!(load_messages(&conn, 7, None, 9).unwrap().is_empty());
        assert_eq!(ids(&load_messages(&conn, 8, None, 9).unwrap()), [6]);
        assert_eq!(ids(&load_messages(&conn, channel, None, 9).unwrap()), [6, 5]);

        // Channel given: only that channel is touched.
        delete_messages(&mut conn, Some(channel), &[6]).unwrap();
        assert_eq!(ids(&load_messages(&conn, channel, None, 9).unwrap()), [5]);
        assert_eq!(ids(&load_messages(&conn, 8, None, 9).unwrap()), [6]);
    }

    #[test]
    fn syncing_a_page_drops_remotely_deleted_messages() {
        let db = store::open(":memory:").unwrap();
        let mut conn = db.lock().unwrap();
        save_messages(&mut conn, &(1..=10).map(message).collect::<Vec<_>>()).unwrap();

        // Full page of 3 covering IDs 7..=10, where 9 is gone on the server.
        let fresh = [message(10), message(8), message(7)];
        sync_history_page(&mut conn, 7, None, 3, &fresh).unwrap();
        assert_eq!(
            ids(&load_messages(&conn, 7, None, 20).unwrap()),
            [10, 8, 7, 6, 5, 4, 3, 2, 1]
        );

        // Short page before 7: the server has only 5 left, the rest are gone.
        sync_history_page(&mut conn, 7, Some(7), 3, &[message(5)]).unwrap();
        assert_eq!(ids(&load_messages(&conn, 7, None, 20).unwrap()), [10, 8, 7, 5]);
    }
}
