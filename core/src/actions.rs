//! What the context menus do to chats and messages: pin, archive, mute,
//! mark unread, clear, delete; react, reply, edit, forward, pin, delete.

use grammers_client::message::InputReactions;
use grammers_client::tl;
use grammers_session::types::{PeerKind, PeerRef};

use crate::chats::{self, resolve_peer, with_db};
use crate::error::UiError;
use crate::store::chats::{self as cache, MessageRow};
use crate::tg::Telegram;

/// Telegram's fixed ID of the Archive folder.
const ARCHIVE_FOLDER: i32 = 1;

fn dialog_peer(peer: &PeerRef) -> tl::enums::InputDialogPeer {
    tl::types::InputDialogPeer { peer: peer.into() }.into()
}

pub async fn pin_dialog(tg: &Telegram, peer_id: i64, pinned: bool) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::messages::ToggleDialogPin {
        pinned,
        peer: dialog_peer(&peer),
    };
    handles.client.invoke(&request).await?;
    Ok(())
}

/// The "unread" mark of a chat, which official clients set by hand.
pub async fn mark_unread(tg: &Telegram, peer_id: i64, unread: bool) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::messages::MarkDialogUnread {
        unread,
        parent_peer: None,
        peer: dialog_peer(&peer),
    };
    handles.client.invoke(&request).await?;
    Ok(())
}

pub async fn archive(tg: &Telegram, peer_id: i64, archived: bool) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::folders::EditPeerFolders {
        folder_peers: vec![
            tl::types::InputFolderPeer {
                peer: (&peer).into(),
                folder_id: if archived { ARCHIVE_FOLDER } else { 0 },
            }
            .into(),
        ],
    };
    handles.client.invoke(&request).await?;
    Ok(())
}

/// Empties the chat for the user only. Not available for channels.
pub async fn clear_history(tg: &Telegram, peer_id: i64) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    if peer.id.kind() == PeerKind::Channel {
        return Err(UiError::new("CHANNEL_INVALID", "channels cannot be cleared"));
    }
    let request = tl::functions::messages::DeleteHistory {
        just_clear: true,
        revoke: false,
        peer: (&peer).into(),
        max_id: 0,
        min_date: None,
        max_date: None,
    };
    // Large chats are cleared in steps; repeat until nothing is left.
    for _ in 0..50 {
        let tl::enums::messages::AffectedHistory::History(done) =
            handles.client.invoke(&request).await?;
        if done.offset <= 0 {
            break;
        }
    }
    with_db(&handles.db, |conn| cache::forget_chat(conn, peer_id))?;
    Ok(())
}

/// Removes the chat from the list: leaves groups and channels, deletes the
/// history of a private chat for the user.
pub async fn delete_chat(tg: &Telegram, peer_id: i64) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    handles.client.delete_dialog(peer).await?;
    with_db(&handles.db, |conn| cache::forget_chat(conn, peer_id))?;
    Ok(())
}

/// Tells the other side the user is typing, as official clients do. Telegram
/// shows it for about six seconds; the window repeats it while typing.
pub async fn set_typing(tg: &Telegram, peer_id: i64) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::messages::SetTyping {
        peer: (&peer).into(),
        top_msg_id: None,
        action: tl::enums::SendMessageAction::SendMessageTypingAction,
    };
    handles.client.invoke(&request).await?;
    Ok(())
}

/// Sets the user's reaction to `emoji`, or removes it when `emoji` is empty.
pub async fn react(tg: &Telegram, peer_id: i64, message_id: i32, emoji: &str) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    // Telegram names its reactions without the "emoji presentation" mark
    // that keyboards and pickers add (the red heart is the usual victim).
    let emoji = emoji.replace('\u{fe0f}', "");
    // A custom emoji already under the message can be added by anyone; it
    // is named "custom:<document ID>" (see `chats::reactions`).
    if emoji == "paid" {
        return Err(UiError::new("REACTION_INVALID", "paid reactions are not sent"));
    }
    let custom = emoji.strip_prefix("custom:").and_then(|id| id.parse::<i64>().ok());
    let reactions = match custom {
        Some(document_id) => InputReactions::custom_emoji(document_id),
        None if emoji.is_empty() => InputReactions::remove(),
        None => InputReactions::emoticon(emoji),
    };
    handles.client.send_reactions(peer, message_id, reactions).await?;
    Ok(())
}

/// The reactions a chat allows: `None` when any emoji goes, otherwise the
/// list its admins chose (empty when reactions are switched off).
pub async fn allowed_reactions(tg: &Telegram, peer_id: i64) -> Result<Option<Vec<String>>, UiError> {
    use tl::enums::{ChatFull, ChatReactions};

    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let available = match peer.id.kind() {
        PeerKind::User => return Ok(None),
        PeerKind::Chat => {
            let request = tl::functions::messages::GetFullChat {
                chat_id: peer.id.bare_id().unwrap_or_default(),
            };
            let tl::enums::messages::ChatFull::Full(full) = handles.client.invoke(&request).await?;
            match full.full_chat {
                ChatFull::Full(chat) => chat.available_reactions,
                ChatFull::ChannelFull(channel) => channel.available_reactions,
                _ => None,
            }
        }
        PeerKind::Channel => {
            let request = tl::functions::channels::GetFullChannel {
                channel: (&peer).into(),
            };
            let tl::enums::messages::ChatFull::Full(full) = handles.client.invoke(&request).await?;
            match full.full_chat {
                ChatFull::Full(chat) => chat.available_reactions,
                ChatFull::ChannelFull(channel) => channel.available_reactions,
                _ => None,
            }
        }
    };
    Ok(match available {
        Some(ChatReactions::All(_)) => None,
        Some(ChatReactions::Some(some)) => Some(
            some.reactions
                .into_iter()
                .filter_map(|reaction| match reaction {
                    tl::enums::Reaction::Emoji(emoji) => Some(emoji.emoticon),
                    _ => None,
                })
                .collect(),
        ),
        Some(ChatReactions::None) | None => Some(Vec::new()),
    })
}

pub async fn edit_message(
    tg: &Telegram,
    peer_id: i64,
    message_id: i32,
    text: &str,
) -> Result<(), UiError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(UiError::new("MESSAGE_EMPTY", "message is empty"));
    }
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let (plain, entities) = crate::markup::parse(text);
    let message = grammers_client::message::InputMessage::new()
        .text(plain)
        .fmt_entities(entities);
    handles.client.edit_message(peer, message_id, message).await?;
    Ok(())
}

/// Deletes messages, for everyone or only for the user. In channels and
/// supergroups deletion is always for everyone.
pub async fn delete_messages(
    tg: &Telegram,
    peer_id: i64,
    ids: &[i32],
    for_everyone: bool,
) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    if peer.id.kind() == PeerKind::Channel {
        handles.client.delete_messages(peer, ids).await?;
    } else {
        let request = tl::functions::messages::DeleteMessages {
            revoke: for_everyone,
            id: ids.to_vec(),
        };
        handles.client.invoke(&request).await?;
    }
    with_db(&handles.db, |conn| cache::delete_messages(conn, Some(peer_id), ids))?;
    Ok(())
}

pub async fn pin_message(
    tg: &Telegram,
    peer_id: i64,
    message_id: i32,
    pinned: bool,
) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    if pinned {
        handles.client.pin_message(peer, message_id).await?;
    } else {
        handles.client.unpin_message(peer, message_id).await?;
    }
    Ok(())
}

/// Forwards messages into another chat and returns them as they appear there.
pub async fn forward(
    tg: &Telegram,
    from_peer_id: i64,
    ids: &[i32],
    to_peer_id: i64,
) -> Result<Vec<MessageRow>, UiError> {
    let handles = tg.handles().await?;
    let source = resolve_peer(&handles, from_peer_id).await?;
    let destination = resolve_peer(&handles, to_peer_id).await?;
    let sent = handles.client.forward_messages(destination, ids, source).await?;
    let rows: Vec<MessageRow> = sent
        .into_iter()
        .flatten()
        .map(|message| chats::message_row(&message, to_peer_id))
        .collect();
    with_db(&handles.db, |conn| cache::save_messages(conn, &rows))?;
    Ok(rows)
}

/// One row of the contacts list.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    /// Also the ID of the private chat with them.
    pub id: i64,
    pub title: String,
    pub photo_id: Option<String>,
    pub presence: Option<cache::Presence>,
}

/// The user's contacts, online ones first, then by name. Their peers are
/// remembered in the session so a chat with any of them can be opened even
/// when there is no conversation yet.
pub async fn contacts(tg: &Telegram) -> Result<Vec<Contact>, UiError> {
    use grammers_session::Session;
    use grammers_session::types::{PeerId, PeerInfo};

    let handles = tg.handles().await?;
    let request = tl::functions::contacts::GetContacts { hash: 0 };
    let tl::enums::contacts::Contacts::Contacts(found) = handles.client.invoke(&request).await?
    else {
        return Ok(Vec::new());
    };
    handles
        .session
        .cache_peers(found.users.iter().cloned().map(PeerInfo::from).collect())
        .await
        .map_err(UiError::internal)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let mut list: Vec<Contact> = found
        .users
        .into_iter()
        .filter_map(|user| match user {
            tl::enums::User::User(user) if !user.is_self && !user.deleted => Some(user),
            _ => None,
        })
        .filter_map(|user| {
            let id = chats::dialog_id(PeerId::user(user.id)?);
            let title = [user.first_name.as_deref(), user.last_name.as_deref()]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let photo_id = match &user.photo {
                Some(tl::enums::UserProfilePhoto::Photo(photo)) => Some(photo.photo_id.to_string()),
                _ => None,
            };
            let presence = user.status.as_ref().map(chats::presence);
            Some(Contact {
                id,
                title,
                photo_id,
                presence,
            })
        })
        .collect();
    let online = |contact: &Contact| {
        contact
            .presence
            .as_ref()
            .is_some_and(|p| p.kind == "online" && p.at > now)
    };
    list.sort_by(|a, b| {
        online(b)
            .cmp(&online(a))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    Ok(list)
}

/// Largest file sent from the window in one go. The bytes pass through
/// memory once, so this bounds the peak; Telegram itself allows 2 GB.
const MAX_SEND_BYTES: usize = 512 * 1024 * 1024;

/// Counts bytes as the uploader reads them, to report progress.
struct Counting<R> {
    inner: R,
    read: u64,
    reported: u64,
    report: Box<dyn Fn(u64) + Send>,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Counting<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        let now = buf.filled().len();
        if now > before {
            self.read += (now - before) as u64;
            // Every 256 KB is plenty for a smooth bar.
            if self.read - self.reported >= 256 * 1024 {
                self.reported = self.read;
                (self.report)(self.read);
            }
        }
        poll
    }
}

/// What the window asks for when sending a file.
pub struct OutgoingFile {
    pub peer_id: i64,
    pub name: String,
    pub caption: String,
    pub reply_to: Option<i32>,
    /// The forum topic the file goes to.
    pub topic: Option<i32>,
    /// Send a picture compressed as a photo rather than as a file.
    pub as_photo: bool,
    /// Ties upload progress events to this file.
    pub token: String,
    /// For a video the window could read: seconds, width, height. With it
    /// the file is sent as a playable video rather than as a plain file.
    pub video: Option<(u64, i32, i32)>,
}

/// Uploads `bytes` and sends them as a photo or a document.
pub async fn send_file(
    tg: &Telegram,
    file: OutgoingFile,
    bytes: Vec<u8>,
) -> Result<MessageRow, UiError> {
    send_file_with_thumb(tg, file, bytes, None).await
}

/// Like [`send_file`]; `thumb` is a small JPEG shown as the preview of a
/// video (ignored for photos and plain files).
pub async fn send_file_with_thumb(
    tg: &Telegram,
    file: OutgoingFile,
    bytes: Vec<u8>,
    thumb: Option<Vec<u8>>,
) -> Result<MessageRow, UiError> {
    use grammers_client::message::InputMessage;

    if file.caption.encode_utf16().count() > 1024 {
        return Err(UiError::new("CAPTION_TOO_LONG", "the caption is too long"));
    }
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, file.peer_id).await?;
    let uploaded = upload(tg, &handles, &file.name, &file.token, bytes).await?;
    let thumb = match thumb.filter(|_| !file.as_photo && file.video.is_some()) {
        Some(bytes) => Some(upload(tg, &handles, "thumb.jpg", &format!("{}#thumb", file.token), bytes).await?),
        None => None,
    };

    let (caption, entities) = crate::markup::parse(file.caption.trim());
    let message = InputMessage::new()
        .text(caption)
        .fmt_entities(entities)
        .reply_to(file.reply_to);
    let message = if file.as_photo {
        message.photo(uploaded)
    } else if let Some((seconds, w, h)) = file.video {
        // Without this attribute Telegram files the upload under documents:
        // no inline player, no preview, in every client.
        let message = message
            .document(uploaded)
            .attribute(grammers_client::media::Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: std::time::Duration::from_secs(seconds),
                w,
                h,
            });
        match thumb {
            Some(thumb) => message.thumbnail(thumb),
            None => message,
        }
    } else {
        message.document(uploaded)
    };
    let sent = handles.client.send_message(peer, message).await?;
    let mut row = chats::message_row(&sent, file.peer_id);
    row.topic_id = file.topic;
    with_db(&handles.db, |conn| cache::save_messages(conn, std::slice::from_ref(&row)))?;
    Ok(row)
}

/// Uploads `bytes` to Telegram, reporting progress under `token`.
async fn upload(
    tg: &Telegram,
    handles: &crate::tg::Handles,
    name: &str,
    token: &str,
    bytes: Vec<u8>,
) -> Result<grammers_client::media::Uploaded, UiError> {
    if bytes.is_empty() {
        return Err(UiError::new("FILE_EMPTY", "the file is empty"));
    }
    if bytes.len() > MAX_SEND_BYTES {
        return Err(UiError::new("FILE_TOO_BIG", "the file is too big"));
    }
    let size = bytes.len();
    let ui = std::sync::Arc::clone(&tg.ui);
    let token = token.to_owned();
    let mut reader = Counting {
        inner: std::io::Cursor::new(bytes),
        read: 0,
        reported: 0,
        report: Box::new(move |done| ui.upload_progress(&token, done)),
    };
    handles
        .client
        .upload_stream(&mut reader, size, name.to_owned())
        .await
        .map_err(|err| UiError::new("UPLOAD_FAILED", err.to_string()))
}

/// Uploads one file of an album and keeps it until `send_album` asks for it.
pub async fn upload_file(tg: &Telegram, name: &str, token: &str, bytes: Vec<u8>) -> Result<(), UiError> {
    upload_file_with_thumb(tg, name, token, bytes, None).await
}

/// Like [`upload_file`]; `thumb` is a small JPEG preview for a video, kept
/// under `"{token}#thumb"` for `send_album`.
pub async fn upload_file_with_thumb(
    tg: &Telegram,
    name: &str,
    token: &str,
    bytes: Vec<u8>,
    thumb: Option<Vec<u8>>,
) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let uploaded = upload(tg, &handles, name, token, bytes).await?;
    let thumb = match thumb {
        Some(bytes) => Some(upload(tg, &handles, "thumb.jpg", &format!("{token}#thumb"), bytes).await?),
        None => None,
    };
    let mut waiting = tg
        .uploads
        .lock()
        .map_err(|_| UiError::internal("uploads lock is poisoned"))?;
    waiting.insert(token.to_owned(), uploaded);
    if let Some(thumb) = thumb {
        waiting.insert(format!("{token}#thumb"), thumb);
    }
    Ok(())
}

/// Forgets uploads that will never be sent (the user cancelled).
pub fn discard_uploads(tg: &Telegram, tokens: &[String]) {
    if let Ok(mut waiting) = tg.uploads.lock() {
        for token in tokens {
            waiting.remove(token);
            waiting.remove(&format!("{token}#thumb"));
        }
    }
}

/// One file of an album, already uploaded under `token`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumItem {
    pub token: String,
    pub as_photo: bool,
    /// "seconds,width,height" for a video, empty otherwise.
    #[serde(default)]
    pub video: String,
}

/// Sends files uploaded with `upload_file` as one album (2 to 10 photos and
/// videos, or 2 to 10 files). The caption goes under the first one.
pub async fn send_album(
    tg: &Telegram,
    peer_id: i64,
    items: Vec<AlbumItem>,
    caption: &str,
    reply_to: Option<i32>,
) -> Result<Vec<MessageRow>, UiError> {
    use grammers_client::media::{Attribute, InputMedia};

    // Whatever happens next, the uploads leave the waiting room.
    let mut uploaded = Vec::with_capacity(items.len());
    let mut thumbs = Vec::with_capacity(items.len());
    {
        let mut waiting = tg
            .uploads
            .lock()
            .map_err(|_| UiError::internal("uploads lock is poisoned"))?;
        for item in &items {
            uploaded.push(waiting.remove(&item.token));
            thumbs.push(waiting.remove(&format!("{}#thumb", item.token)));
        }
    }
    if caption.encode_utf16().count() > 1024 {
        return Err(UiError::new("CAPTION_TOO_LONG", "the caption is too long"));
    }
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let (caption, entities) = crate::markup::parse(caption.trim());

    let mut medias = Vec::with_capacity(items.len());
    for (index, ((item, file), thumb)) in items.iter().zip(uploaded).zip(thumbs).enumerate() {
        let file = file.ok_or_else(|| UiError::new("UPLOAD_FAILED", "the upload is gone"))?;
        let mut media = InputMedia::new();
        if index == 0 {
            media = media.caption(caption.clone()).fmt_entities(entities.clone()).reply_to(reply_to);
        }
        media = if item.as_photo {
            media.photo(file)
        } else if let Some((seconds, w, h)) = parse_video(&item.video) {
            let media = media.document(file).attribute(Attribute::Video {
                round_message: false,
                supports_streaming: true,
                duration: std::time::Duration::from_secs(seconds),
                w,
                h,
            });
            match thumb {
                Some(thumb) => media.thumbnail(thumb),
                None => media,
            }
        } else {
            media.document(file)
        };
        medias.push(media);
    }

    let sent = handles.client.send_album(peer, medias).await?;
    let rows: Vec<MessageRow> = sent
        .iter()
        .flatten()
        .map(|message| chats::message_row(message, peer_id))
        .collect();
    with_db(&handles.db, |conn| cache::save_messages(conn, &rows))?;
    Ok(rows)
}

/// "seconds,width,height" of a video, as the window measured them.
pub fn parse_video(value: &str) -> Option<(u64, i32, i32)> {
    let mut parts = value.split(',');
    let seconds = parts.next()?.parse().ok()?;
    let width: i32 = parts.next()?.parse().ok()?;
    let height: i32 = parts.next()?.parse().ok()?;
    (width > 0 && height > 0).then_some((seconds, width, height))
}

/// A chat found by its public @username.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundChat {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub photo_id: Option<String>,
    /// The public @username, without "@".
    pub username: Option<String>,
    /// Subscribers or members, when Telegram tells.
    pub members: Option<i32>,
    /// A channel or supergroup the user has not joined (it can be read, and
    /// joined with `join_channel`).
    pub joinable: bool,
}

/// People, groups and channels by name (contacts.search): own chats and
/// contacts first, then public ones. Each is remembered, so it can be
/// opened right away.
pub async fn search_peers(tg: &Telegram, query: &str, limit: i32) -> Result<Vec<FoundChat>, UiError> {
    use grammers_session::Session;
    use grammers_session::types::{PeerId, PeerInfo};

    let query = query.trim();
    if query.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let handles = tg.handles().await?;
    let request = tl::functions::contacts::Search {
        broadcasts: false,
        bots: false,
        q: query.to_owned(),
        limit,
    };
    let tl::enums::contacts::Found::Found(found) = handles.client.invoke(&request).await?;

    // What the answer says about each peer, by its dialog ID.
    let mut about: std::collections::HashMap<i64, FoundChat> = std::collections::HashMap::new();
    for user in &found.users {
        let tl::enums::User::User(u) = user else { continue };
        let Some(id) = PeerId::user(u.id).map(chats::dialog_id) else { continue };
        let title = [u.first_name.as_deref(), u.last_name.as_deref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let photo_id = match &u.photo {
            Some(tl::enums::UserProfilePhoto::Photo(p)) => Some(p.photo_id.to_string()),
            _ => None,
        };
        let kind = if u.is_self { "saved" } else { "user" };
        about.insert(id, FoundChat {
            id,
            kind: kind.into(),
            title,
            photo_id,
            username: u.username.clone(),
            members: None,
            joinable: false,
        });
    }
    for chat in &found.chats {
        match chat {
            tl::enums::Chat::Channel(c) => {
                let Some(id) = PeerId::channel(c.id).map(chats::dialog_id) else { continue };
                let photo_id = match &c.photo {
                    tl::enums::ChatPhoto::Photo(p) => Some(p.photo_id.to_string()),
                    _ => None,
                };
                about.insert(id, FoundChat {
                    id,
                    kind: if c.broadcast { "channel" } else { "group" }.into(),
                    title: c.title.clone(),
                    photo_id,
                    username: c.username.clone(),
                    members: c.participants_count,
                    joinable: c.left,
                });
            }
            tl::enums::Chat::Chat(c) => {
                let Some(id) = PeerId::chat(c.id).map(chats::dialog_id) else { continue };
                let photo_id = match &c.photo {
                    tl::enums::ChatPhoto::Photo(p) => Some(p.photo_id.to_string()),
                    _ => None,
                };
                about.insert(id, FoundChat {
                    id,
                    kind: "group".into(),
                    title: c.title.clone(),
                    photo_id,
                    username: None,
                    members: Some(c.participants_count),
                    joinable: false,
                });
            }
            _ => {}
        }
    }

    let mut peers: Vec<PeerInfo> = found.users.into_iter().map(PeerInfo::from).collect();
    peers.extend(found.chats.into_iter().map(PeerInfo::from));
    handles.session.cache_peers(peers).await.map_err(UiError::internal)?;

    let mut out = Vec::new();
    for peer in found.my_results.into_iter().chain(found.results) {
        let id = chats::dialog_id(PeerId::from(peer));
        if out.iter().any(|f: &FoundChat| f.id == id) {
            continue;
        }
        if let Some(chat) = about.remove(&id) {
            out.push(chat);
        }
    }
    Ok(out)
}

/// What a bot answered to a pressed callback button.
pub struct ButtonAnswer {
    pub message: Option<String>,
    /// Show `message` as a dialog rather than a toast.
    pub alert: bool,
    pub url: Option<String>,
}

/// Presses a callback button (`data` in hex) of a message.
pub async fn press_button(tg: &Telegram, peer_id: i64, message_id: i32, data: &str) -> Result<ButtonAnswer, UiError> {
    let data: Vec<u8> = (0..data.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(data.get(i..i + 2)?, 16).ok())
        .collect();
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::messages::GetBotCallbackAnswer {
        game: false,
        peer: peer.into(),
        msg_id: message_id,
        data: Some(data),
        password: None,
    };
    let tl::enums::messages::BotCallbackAnswer::Answer(answer) = handles.client.invoke(&request).await?;
    Ok(ButtonAnswer { message: answer.message.filter(|m| !m.is_empty()), alert: answer.alert, url: answer.url })
}

/// Joins a public channel or supergroup ("Подписаться" / "Вступить").
pub async fn join_channel(tg: &Telegram, peer_id: i64) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    handles.client.join_chat(peer).await?;
    Ok(())
}

/// Finds a user, group or channel by @username (as in mentions and t.me
/// links) and remembers it, so its chat can be opened right away.
pub async fn resolve_username(tg: &Telegram, username: &str) -> Result<FoundChat, UiError> {
    use grammers_session::Session;
    use grammers_session::types::{PeerId, PeerInfo};

    let username = username.trim().trim_start_matches('@');
    if username.is_empty() {
        return Err(UiError::new("USERNAME_INVALID", "empty username"));
    }
    let handles = tg.handles().await?;
    let request = tl::functions::contacts::ResolveUsername {
        username: username.to_owned(),
        referer: None,
    };
    let tl::enums::contacts::ResolvedPeer::Peer(found) = handles.client.invoke(&request).await?;
    let mut peers: Vec<PeerInfo> = found.users.into_iter().map(PeerInfo::from).collect();
    peers.extend(found.chats.into_iter().map(PeerInfo::from));
    handles
        .session
        .cache_peers(peers)
        .await
        .map_err(UiError::internal)?;

    let id = chats::dialog_id(PeerId::from(found.peer));
    let peer_ref = resolve_peer(&handles, id).await?;
    let peer = handles.client.resolve_peer(peer_ref).await?;
    let summary = chats::peer_summary(Some(&peer));
    let (username, joinable) = match &peer {
        grammers_client::peer::Peer::Channel(c) => (c.username().map(str::to_owned), c.raw.left),
        grammers_client::peer::Peer::Group(g) => (g.username().map(str::to_owned), matches!(&g.raw, tl::enums::Chat::Channel(raw) if raw.left)),
        grammers_client::peer::Peer::User(u) => (u.username().map(str::to_owned), false),
        _ => (None, false),
    };
    Ok(FoundChat {
        id,
        kind: summary.kind,
        title: summary.title,
        photo_id: summary.photo_id,
        username,
        members: None,
        joinable,
    })
}

/// Messages in one chat containing `query`, newest first.
pub async fn search_chat(
    tg: &Telegram,
    peer_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<MessageRow>, UiError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let mut iter = handles
        .client
        .search_messages(peer)
        .query(query)
        .limit(limit.clamp(1, 100));
    let mut rows = Vec::new();
    while let Some(message) = iter.next().await? {
        rows.push(chats::message_row(&message, peer_id));
    }
    Ok(rows)
}

/// A message found by the global search, with the chat it is in.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundMessage {
    pub message: MessageRow,
    pub peer: chats::PeerSummary,
}

/// Messages in all chats containing `query`, newest first.
pub async fn search_global(
    tg: &Telegram,
    query: &str,
    limit: usize,
) -> Result<Vec<FoundMessage>, UiError> {
    let query = query.trim();
    if query.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let handles = tg.handles().await?;
    let mut iter = handles
        .client
        .search_all_messages()
        .query(query)
        .limit(limit.clamp(1, 100));
    let mut found = Vec::new();
    while let Some(message) = iter.next().await? {
        let peer_id = chats::dialog_id(message.peer_id());
        found.push(FoundMessage {
            message: chats::message_row(&message, peer_id),
            peer: chats::peer_summary(message.peer()),
        });
    }
    Ok(found)
}
