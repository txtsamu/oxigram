//! Dialog list, message history, sending and pictures: network calls plus
//! the cache.

use std::path::PathBuf;

use grammers_client::message::Message;
use grammers_client::peer::{Dialog, Peer};
use grammers_client::tl;
use grammers_session::Session;
use grammers_session::types::{PeerId, PeerKind, PeerRef};
use serde::Serialize;

use crate::error::UiError;
use crate::media;
use crate::service;
use crate::store::Db;
use crate::store::chats::{self as cache, DialogRow, MessageRow, Presence};
use crate::tg::{DialogCursor, Handles, Telegram};

const DIALOG_PAGE: usize = 50;
const MAX_HISTORY_PAGE: usize = 100;
/// Telegram's limit for one text message, in UTF-16 code units.
const MAX_MESSAGE_LEN: usize = 4096;
/// Telegram's fixed ID of the Archive folder.
const ARCHIVE_FOLDER: i32 = 1;

#[derive(Serialize)]
pub struct DialogPage {
    pub dialogs: Vec<DialogRow>,
    /// No more dialogs on the server after this page.
    pub done: bool,
}

/// How a chat is presented: enough to draw its row in the list.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSummary {
    pub kind: String,
    pub title: String,
    pub photo_id: Option<String>,
}

/// A forum topic, as the topic list shows it.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicRow {
    pub id: i32,
    pub title: String,
    pub unread: i32,
    pub pinned: bool,
    pub closed: bool,
    pub icon_color: i32,
    pub icon_emoji_id: Option<String>,
}

/// The dialog list as it was last seen, for an instant first paint.
pub async fn cached_dialogs(tg: &Telegram) -> Result<Vec<DialogRow>, UiError> {
    let Handles { db, .. } = tg.handles().await?;
    with_db(&db, |conn| cache::load_dialogs(conn))
}

/// Fetches the next page of dialogs; `reset` starts over from the top.
pub async fn load_dialogs(tg: &Telegram, reset: bool) -> Result<DialogPage, UiError> {
    let started = std::time::Instant::now();
    let page = load_dialogs_page(tg, reset).await;
    if reset {
        crate::timing::mark("dialogs: first page (messages.getDialogs)", started);
    }
    page
}

async fn load_dialogs_page(tg: &Telegram, reset: bool) -> Result<DialogPage, UiError> {
    let Handles { client, db, .. } = tg.handles().await?;
    let mut guard = tg.dialog_cursor.lock().await;
    if reset || guard.is_none() {
        *guard = Some(DialogCursor {
            iter: client.iter_dialogs(),
            loaded: 0,
        });
    }
    let Some(cursor) = guard.as_mut() else {
        return Err(UiError::internal("dialog cursor unavailable"));
    };

    let mut rows = Vec::with_capacity(DIALOG_PAGE);
    let mut done = false;
    while rows.len() < DIALOG_PAGE {
        match cursor.iter.next().await {
            // Official clients hide groups that no longer exist as such.
            Ok(Some(dialog)) if is_defunct(&dialog) => {}
            Ok(Some(dialog)) => rows.push(dialog_row(&dialog)),
            Ok(None) => {
                done = true;
                break;
            }
            Err(err) => {
                // The iterator's position is unknown after a failure.
                *guard = None;
                return Err(err.into());
            }
        }
    }

    let start = cursor.loaded;
    cursor.loaded += rows.len();
    with_db(&db, |conn| cache::save_dialogs(conn, start, &rows))?;
    Ok(DialogPage {
        dialogs: rows,
        done,
    })
}

/// Cached messages older than `before_id`, newest first.
pub async fn cached_history(
    tg: &Telegram,
    peer_id: i64,
    before_id: Option<i32>,
    limit: usize,
) -> Result<Vec<MessageRow>, UiError> {
    let Handles { db, .. } = tg.handles().await?;
    let limit = limit.clamp(1, MAX_HISTORY_PAGE);
    with_db(&db, |conn| cache::load_messages(conn, peer_id, before_id, limit))
}

/// Fetches messages older than `before_id` from Telegram, newest first, and
/// brings the cache in line with what the server returned.
pub async fn history(
    tg: &Telegram,
    peer_id: i64,
    before_id: Option<i32>,
    limit: usize,
) -> Result<Vec<MessageRow>, UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let limit = limit.clamp(1, MAX_HISTORY_PAGE);

    let mut iter = handles.client.iter_messages(peer).limit(limit);
    if let Some(before_id) = before_id {
        iter = iter.offset_id(before_id);
    }
    let mut rows = Vec::with_capacity(limit);
    while let Some(message) = iter.next().await? {
        rows.push(message_row(&message, peer_id));
    }

    with_db(&handles.db, |conn| {
        cache::sync_history_page(conn, peer_id, before_id, limit, &rows)
    })?;
    Ok(rows)
}

/// The forum topics of a group, pinned first as Telegram orders them.
pub async fn topics(tg: &Telegram, peer_id: i64) -> Result<Vec<TopicRow>, UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::messages::GetForumTopics {
        peer: (&peer).into(),
        q: None,
        offset_date: 0,
        offset_id: 0,
        offset_topic: 0,
        limit: 100,
    };
    let tl::enums::messages::ForumTopics::Topics(found) = handles.client.invoke(&request).await?;
    Ok(found
        .topics
        .into_iter()
        .filter_map(|topic| match topic {
            tl::enums::ForumTopic::Topic(topic) => Some(TopicRow {
                id: topic.id,
                title: topic.title,
                unread: topic.unread_count,
                pinned: topic.pinned,
                closed: topic.closed,
                icon_color: topic.icon_color,
                icon_emoji_id: topic.icon_emoji_id.map(|id| id.to_string()),
            }),
            _ => None,
        })
        .collect())
}

/// Messages of one forum topic, newest first, older than `before_id`.
pub async fn topic_history(
    tg: &Telegram,
    peer_id: i64,
    topic_id: i32,
    before_id: Option<i32>,
    limit: usize,
) -> Result<Vec<MessageRow>, UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let limit = limit.clamp(1, MAX_HISTORY_PAGE);
    let request = tl::functions::messages::GetReplies {
        peer: (&peer).into(),
        msg_id: topic_id,
        offset_id: before_id.unwrap_or(0),
        offset_date: 0,
        add_offset: 0,
        limit: limit as i32,
        max_id: 0,
        min_id: 0,
        hash: 0,
    };
    let page = match handles.client.invoke(&request).await? {
        tl::enums::messages::Messages::Messages(page) => page.messages,
        tl::enums::messages::Messages::Slice(page) => page.messages,
        tl::enums::messages::Messages::ChannelMessages(page) => page.messages,
        tl::enums::messages::Messages::NotModified(_) => Vec::new(),
    };
    let ids: Vec<i32> = page
        .into_iter()
        .filter_map(|message| match message {
            tl::enums::Message::Message(message) => Some(message.id),
            _ => None,
        })
        .collect();
    let mut rows = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(100) {
        let found = handles.client.get_messages_by_id(peer, chunk).await?;
        for message in found.into_iter().flatten() {
            rows.push(message_row(&message, peer_id));
        }
    }
    Ok(rows)
}

/// One page of a chat's shared media of one kind ("media" = photos and
/// videos, "file", "link", "voice", "audio"), newest first, older than
/// `before_id`. Not cached: the profile panel asks for it on demand.
pub async fn shared_media(
    tg: &Telegram,
    peer_id: i64,
    kind: &str,
    before_id: Option<i32>,
    limit: usize,
) -> Result<Vec<MessageRow>, UiError> {
    use tl::enums::MessagesFilter as F;
    let filter = match kind {
        "media" => F::InputMessagesFilterPhotoVideo,
        "file" => F::InputMessagesFilterDocument,
        "link" => F::InputMessagesFilterUrl,
        "voice" => F::InputMessagesFilterRoundVoice,
        "audio" => F::InputMessagesFilterMusic,
        "pinned" => F::InputMessagesFilterPinned,
        _ => return Err(UiError::new("BAD_REQUEST", "unknown media kind")),
    };
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let limit = limit.clamp(1, MAX_HISTORY_PAGE);
    let mut iter = handles
        .client
        .search_messages(peer)
        .filter(filter)
        .limit(limit);
    if let Some(before_id) = before_id {
        iter = iter.offset_id(before_id);
    }
    let mut rows = Vec::with_capacity(limit);
    while let Some(message) = iter.next().await? {
        rows.push(message_row(&message, peer_id));
    }
    Ok(rows)
}

/// Turns notifications for a chat off for good, or back on.
pub async fn set_muted(tg: &Telegram, peer_id: i64, muted: bool) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let request = tl::functions::account::UpdateNotifySettings {
        peer: tl::types::InputNotifyPeer {
            peer: (&peer).into(),
        }
        .into(),
        settings: tl::types::InputPeerNotifySettings {
            show_previews: None,
            silent: None,
            // Telegram's "forever" is the largest 32-bit time.
            mute_until: Some(if muted { i32::MAX } else { 0 }),
            sound: None,
            stories_muted: None,
            stories_hide_sender: None,
            stories_sound: None,
        }
        .into(),
    };
    handles.client.invoke(&request).await?;
    with_db(&handles.db, |conn| cache::set_muted(conn, peer_id, muted))?;
    Ok(())
}

pub async fn send_message(
    tg: &Telegram,
    peer_id: i64,
    text: &str,
    reply_to: Option<i32>,
    topic: Option<i32>,
) -> Result<MessageRow, UiError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(UiError::new("MESSAGE_EMPTY", "message is empty"));
    }
    if text.encode_utf16().count() > MAX_MESSAGE_LEN {
        return Err(UiError::new("MESSAGE_TOO_LONG", "message is too long"));
    }

    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    // **bold**, __italic__, `code`… as typed in Telegram Desktop.
    let (plain, entities) = crate::markup::parse(text);
    let message = grammers_client::message::InputMessage::new()
        .text(plain.clone())
        .fmt_entities(entities.clone())
        .reply_to(reply_to);
    let sent = match topic {
        None => handles.client.send_message(peer, message).await?,
        Some(topic) => {
            let request = tl::functions::messages::SendMessage {
                no_webpage: false,
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: (&peer).into(),
                reply_to: Some(tl::enums::InputReplyTo::Message(tl::types::InputReplyToMessage {
                    reply_to_msg_id: reply_to.unwrap_or(topic),
                    top_msg_id: Some(topic),
                    reply_to_peer_id: None,
                    quote_text: None,
                    quote_entities: None,
                    quote_offset: None,
                    monoforum_peer_id: None,
                    todo_item_id: None,
                    poll_option: None,
                    quote_rich: None,
                })),
                message: plain,
                random_id: random_id(),
                reply_markup: None,
                entities: Some(entities),
                schedule_date: None,
                schedule_repeat_period: None,
                send_as: None,
                quick_reply_shortcut: None,
                effect: None,
                allow_paid_stars: None,
                suggested_post: None,
                rich_message: None,
            };
            let updates = handles.client.invoke(&request).await?;
            match sent_id(&updates) {
                Some(id) => handles
                    .client
                    .get_messages_by_id(peer, &[id])
                    .await?
                    .into_iter()
                    .flatten()
                    .next()
                    .ok_or_else(|| UiError::internal("the sent message was not found"))?,
                None => return Err(UiError::internal("Telegram did not return the sent message")),
            }
        }
    };
    let mut row = message_row(&sent, peer_id);
    // The short update for a sent message carries no reply header.
    if row.reply_to_id.is_none() {
        row.reply_to_id = reply_to;
    }
    row.topic_id = topic;
    with_db(&handles.db, |conn| {
        cache::save_messages(conn, std::slice::from_ref(&row))
    })?;
    Ok(row)
}

/// Messages of a chat by ID (for the quotes of replies to messages that
/// are not loaded), each with its row or `None` when it no longer exists.
pub async fn messages_by_id(
    tg: &Telegram,
    peer_id: i64,
    ids: &[i32],
) -> Result<Vec<(i32, Option<MessageRow>)>, UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let mut out = Vec::with_capacity(ids.len());
    // Telegram takes up to 100 IDs per request.
    for chunk in ids.chunks(100) {
        let found = handles.client.get_messages_by_id(peer, chunk).await?;
        for (id, message) in chunk.iter().zip(found) {
            out.push((*id, message.map(|m| message_row(&m, peer_id))));
        }
    }
    Ok(out)
}

/// Tells Telegram the chat was read, as official clients do when it is opened.
pub async fn mark_read(tg: &Telegram, peer_id: i64) -> Result<(), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    handles.client.mark_as_read(peer).await?;
    with_db(&handles.db, |conn| cache::clear_unread(conn, peer_id))
}

/// Path of the picture for a message (photo thumbnail, sticker or preview
/// frame), downloading it on first use.
pub async fn message_image(tg: &Telegram, peer_id: i64, message_id: i32) -> Result<String, UiError> {
    let dir = tg.cache_dir.join("messages");
    // The format is only known once the message is fetched, so look for both.
    for ext in ["jpg", "webp"] {
        let path = dir.join(format!("{peer_id}_{message_id}.{ext}"));
        if path.is_file() {
            return Ok(path_string(path));
        }
    }

    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let _permit = tg.downloads.acquire().await.map_err(UiError::internal)?;
    // Refetching the message also yields a fresh file reference.
    let message = handles
        .client
        .get_messages_by_id(peer, &[message_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(no_picture)?;
    let source = media::describe(&message).image.ok_or_else(no_picture)?;
    let path = dir.join(format!("{peer_id}_{message_id}.{}", source.ext));
    media::fetch(&handles.client, &source, &path).await?;
    Ok(path_string(path))
}

/// Path of a chat's profile photo, downloading it on first use. A new photo
/// has a new ID and therefore a new file.
pub async fn avatar(tg: &Telegram, peer_id: i64, photo_id: &str) -> Result<String, UiError> {
    let photo: i64 = photo_id.parse().map_err(|_| no_picture())?;
    let path = tg
        .cache_dir
        .join("avatars")
        .join(format!("{peer_id}_{photo}.jpg"));
    if path.is_file() {
        return Ok(path_string(path));
    }

    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let _permit = tg.downloads.acquire().await.map_err(UiError::internal)?;
    media::fetch(&handles.client, &media::avatar(peer, photo), &path).await?;
    Ok(path_string(path))
}

/// Path of a custom emoji's picture, downloading it on first use.
pub async fn custom_emoji(tg: &Telegram, id: &str) -> Result<String, UiError> {
    let document_id: i64 = id.parse().map_err(|_| no_picture())?;
    let path = tg.cache_dir.join("emoji").join(format!("{document_id}.webp"));
    if path.is_file() {
        return Ok(path_string(path));
    }

    let handles = tg.handles().await?;
    let _permit = tg.downloads.acquire().await.map_err(UiError::internal)?;
    let documents = handles
        .client
        .invoke(&tl::functions::messages::GetCustomEmojiDocuments {
            document_id: vec![document_id],
        })
        .await?;
    let source = documents
        .iter()
        .find_map(|document| match document {
            tl::enums::Document::Document(document) => media::custom_emoji(document),
            tl::enums::Document::Empty(_) => None,
        })
        .ok_or_else(no_picture)?;
    media::fetch(&handles.client, &source, &path).await?;
    Ok(path_string(path))
}

/// Path of the full file behind a message (video, audio, original photo),
/// downloading it on first use.
/// Downloads the full file behind a message into the cache, reporting the
/// bytes received so far through `progress`.
pub async fn message_file(
    tg: &Telegram,
    peer_id: i64,
    message_id: i32,
    progress: impl Fn(u64),
) -> Result<String, UiError> {
    let dir = tg.cache_dir.join("files");
    for ext in ["mp4", "webm", "ogg", "mp3", "m4a", "jpg", "png", "webp"] {
        let path = dir.join(format!("{peer_id}_{message_id}.{ext}"));
        if path.is_file() {
            return Ok(path_string(path));
        }
    }

    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let message = handles
        .client
        .get_messages_by_id(peer, &[message_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(no_picture)?;
    let ext = media::describe(&message).file_ext.ok_or_else(no_picture)?;
    let media = media::media_of(&message).ok_or_else(no_picture)?;

    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{peer_id}_{message_id}.{ext}"));
    // Download under another name so an interrupted file is never played.
    let partial = path.with_extension("part");
    let result = download_to(&handles, &media, &partial, progress).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result?;
    std::fs::rename(&partial, &path)?;
    trim_cache(&dir, FILE_CACHE_LIMIT, &path);
    Ok(path_string(path))
}

/// The discussion group of a channel post: the chat and the message in it
/// that holds the comments to `message_id`.
pub async fn discussion(tg: &Telegram, peer_id: i64, message_id: i32) -> Result<(i64, i32), UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let tl::enums::messages::DiscussionMessage::Message(discussion) = handles
        .client
        .invoke(&tl::functions::messages::GetDiscussionMessage { peer: peer.into(), msg_id: message_id })
        .await?;
    // The first message is the automatic forward of the post that starts the
    // thread; its chat is the discussion group.
    let Some(tl::enums::Message::Message(raw)) = discussion.messages.into_iter().next() else {
        return Err(UiError::new("NO_DISCUSSION", "the post has no discussion group"));
    };
    Ok((dialog_id(PeerId::from(raw.peer_id)), raw.id))
}

/// Videos and audio kept for replay; beyond this the oldest go first.
const FILE_CACHE_LIMIT: u64 = 500 * 1024 * 1024;
/// Deletes the least recently written files in `dir` until it holds at most
/// `limit` bytes. `keep` (the file just fetched) is never deleted.
fn trim_cache(dir: &std::path::Path, limit: u64, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        if path != keep && std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// Downloads `media` to `path`, several 512 KB chunks at a time (as
/// official clients do: one request after another is slow on a far data
/// centre), reporting the bytes received so far. Memory stays at a few chunks.
async fn download_to(
    handles: &Handles,
    media: &grammers_client::media::Media,
    path: &std::path::Path,
    progress: impl Fn(u64),
) -> Result<(), UiError> {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

    /// grammers' chunk size for `iter_download`.
    const CHUNK: u64 = 512 * 1024;

    let file = std::fs::File::create(path)?;
    let Some(size) = media.size().map(|size| size as u64).filter(|size| *size > CHUNK) else {
        // Small or of unknown size: one request after another is fine.
        let mut file = std::io::BufWriter::new(file);
        let mut download = handles.client.iter_download(media);
        let mut done = 0;
        while let Some(chunk) = download.next().await? {
            file.write_all(&chunk)?;
            done += chunk.len() as u64;
            progress(done);
        }
        file.flush()?;
        return Ok(());
    };

    file.set_len(size)?;
    let file = Mutex::new(file);
    let parts = size.div_ceil(CHUNK) as i32;
    let next = AtomicI32::new(0);
    let done = AtomicU64::new(0);

    // Each worker takes the next chunk index and fetches just that chunk.
    // A fresh iterator per chunk lets grammers handle data-centre moves and
    // authorization there, as it does for whole files.
    let worker = || async {
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            if index >= parts {
                return Ok::<(), UiError>(());
            }
            let chunk = handles
                .client
                .iter_download(media)
                .skip_chunks(index)
                .next()
                .await?
                .unwrap_or_default();
            {
                let mut file = file.lock().map_err(|_| UiError::new("STORAGE", "file lock"))?;
                file.seek(SeekFrom::Start(u64::from(index as u32) * CHUNK))?;
                file.write_all(&chunk)?;
            }
            let total = done.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
            progress(total);
        }
    };
    tokio::try_join!(worker(), worker(), worker(), worker())?;
    Ok(())
}

/// Downloads the attachment of a message into `dir` under its own name (made
/// unique) and returns the full path, reporting the bytes received so far
/// through `progress`. `name` overrides the file name ("Сохранить как…").
/// Self-destructing media is refused, as official clients do. The file is
/// written as `*.oxipart` and renamed when complete; when the download fails
/// or its task is dropped (cancelled), the partial file is removed.
pub async fn save_file(
    tg: &Telegram,
    peer_id: i64,
    message_id: i32,
    dir: PathBuf,
    name: Option<String>,
    progress: impl Fn(u64),
) -> Result<String, UiError> {
    let handles = tg.handles().await?;
    let peer = resolve_peer(&handles, peer_id).await?;
    let message = handles
        .client
        .get_messages_by_id(peer, &[message_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(no_file)?;
    let media = media::media_of(&message).ok_or_else(no_file)?;
    if media::is_ephemeral(&media) {
        return Err(UiError::new(
            "MEDIA_EPHEMERAL",
            "self-destructing media cannot be saved",
        ));
    }
    let info = media::describe(&message);
    // A name from the save dialog is used as is (the dialog already asked
    // about replacing a file); the default one is made unique.
    let chosen = name.is_some();
    let name = file_name(name.as_deref().unwrap_or(&info.file_name), info.kind, info.file_ext, message_id);

    std::fs::create_dir_all(&dir)?;
    let path = if chosen { dir.join(&name) } else { unique_path(&dir, &name) };
    let partial = path.with_extension(format!(
        "{}oxipart",
        path.extension()
            .map(|ext| format!("{}.", ext.to_string_lossy()))
            .unwrap_or_default()
    ));
    let guard = RemoveOnDrop(Some(partial.clone()));
    download_to(&handles, &media, &partial, progress).await?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(&partial, &path)?;
    std::mem::forget(guard);
    Ok(path_string(path))
}

/// Removes a partial download when the download stops before the end.
struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A safe file name: the sender's name with characters Windows refuses
/// replaced, or a made-up one from the message ID.
fn file_name(original: &str, kind: &str, ext: Option<&str>, message_id: i32) -> String {
    let cleaned: String = original
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_owned();
    if !cleaned.is_empty() {
        return cleaned;
    }
    let base = match kind {
        "photo" => "photo",
        "video" | "gif" | "round" => "video",
        "voice" => "voice",
        "audio" => "audio",
        _ => "file",
    };
    format!("{base}_{message_id}.{}", ext.unwrap_or("bin"))
}

/// `name`, or `name (1)`, `name (2)`… if a file of that name already exists.
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|path| !path.exists())
        .expect("some name is free")
}

fn no_file() -> UiError {
    UiError::new("MEDIA_EMPTY", "the file is not available")
}

/// What the profile panel shows about a chat.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub photo_id: Option<String>,
    pub username: Option<String>,
    pub phone: Option<String>,
    pub about: String,
    pub members: Option<i32>,
    /// Members online now (supergroups and channels).
    pub online: Option<i32>,
    /// "13.11" or "13.11.1990".
    pub birthday: Option<String>,
    /// Groups shared with this user.
    pub common_chats: Option<i32>,
    /// The chat's invite link, when the user may see it.
    pub invite_link: Option<String>,
    pub muted: bool,
    /// The group is a forum: its chats are split into topics.
    pub forum: bool,
    /// Shared media, files and links in the chat, non-zero counts only.
    pub media: Vec<MediaCount>,
}

#[derive(Serialize)]
pub struct MediaCount {
    pub kind: &'static str,
    pub count: i32,
}

/// The shared-media sections of a profile, in Telegram's order.
const MEDIA_FILTERS: [(&str, tl::enums::MessagesFilter); 7] = [
    ("photo", tl::enums::MessagesFilter::InputMessagesFilterPhotos),
    ("video", tl::enums::MessagesFilter::InputMessagesFilterVideo),
    ("file", tl::enums::MessagesFilter::InputMessagesFilterDocument),
    ("audio", tl::enums::MessagesFilter::InputMessagesFilterMusic),
    ("link", tl::enums::MessagesFilter::InputMessagesFilterUrl),
    ("voice", tl::enums::MessagesFilter::InputMessagesFilterVoice),
    ("gif", tl::enums::MessagesFilter::InputMessagesFilterGif),
];

fn is_muted(settings: &tl::enums::PeerNotifySettings) -> bool {
    let tl::enums::PeerNotifySettings::Settings(settings) = settings;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    settings.mute_until.is_some_and(|until| i64::from(until) > now)
}

fn invite_link(invite: Option<tl::enums::ExportedChatInvite>) -> Option<String> {
    match invite? {
        tl::enums::ExportedChatInvite::ChatInviteExported(invite) => Some(invite.link),
        _ => None,
    }
}

/// Commands the bots of a chat advertise, as (bot, command, description).
///
/// A private chat with a bot reads them from its full profile; a group has to
/// find the bots among its members first.
pub async fn bot_commands(
    tg: &Telegram,
    peer_id: i64,
) -> Result<Vec<(String, String, String)>, UiError> {
    let handles = tg.handles().await?;
    let peer_ref = resolve_peer(&handles, peer_id).await?;
    let mut bots: Vec<(String, i64, tl::enums::InputUser)> = Vec::new();
    match peer_ref.id.kind() {
        PeerKind::User => bots.push((
            String::new(),
            peer_ref.id.bare_id().unwrap_or_default(),
            (&peer_ref).into(),
        )),
        PeerKind::Channel => {
            let request = tl::functions::channels::GetParticipants {
                channel: (&peer_ref).into(),
                filter: tl::enums::ChannelParticipantsFilter::ChannelParticipantsBots,
                offset: 0,
                limit: 100,
                hash: 0,
            };
            let list = handles.client.invoke(&request).await?;
            if let tl::enums::channels::ChannelParticipants::Participants(list) = list {
                for user in list.users {
                    let tl::enums::User::User(user) = user else {
                        continue;
                    };
                    bots.push((
                        user.first_name.clone().unwrap_or_default(),
                        user.id,
                        tl::enums::InputUser::User(tl::types::InputUser {
                            user_id: user.id,
                            access_hash: user.access_hash.unwrap_or_default(),
                        }),
                    ));
                }
            }
        }
        _ => {}
    }

    let mut commands = Vec::new();
    for (name, user_id, bot) in bots {
        let request = tl::functions::users::GetFullUser { id: bot };
        let Ok(tl::enums::users::UserFull::Full(full)) = handles.client.invoke(&request).await else {
            continue;
        };
        let tl::enums::UserFull::Full(user) = full.full_user;
        let name = if name.is_empty() {
            full.users
                .iter()
                .find_map(|user| match user {
                    tl::enums::User::User(user) if user.id == user_id => {
                        Some(user.first_name.clone().unwrap_or_default())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            name
        };
        let Some(tl::enums::BotInfo::Info(info)) = user.bot_info else {
            continue;
        };
        for command in info.commands.unwrap_or_default() {
            let tl::enums::BotCommand::Command(command) = command;
            commands.push((name.clone(), command.command, command.description));
        }
    }
    Ok(commands)
}

pub async fn peer_info(tg: &Telegram, peer_id: i64) -> Result<PeerInfo, UiError> {
    let handles = tg.handles().await?;
    let peer_ref = resolve_peer(&handles, peer_id).await?;
    let peer = handles.client.resolve_peer(peer_ref).await?;
    let mut summary = peer_summary(Some(&peer));
    let phone = match &peer {
        Peer::User(user) => user.phone().map(str::to_owned),
        _ => None,
    };
    // As "Saved Messages" the own account has no name or photo; the profile
    // view needs both.
    if let Peer::User(user) = &peer {
        if user.is_self() {
            summary.title = display_name(&peer);
            summary.photo_id = user.photo().map(|photo| photo.photo_id.to_string());
        }
    }

    use tl::enums::ChatFull;
    let mut extra = Extra::default();
    // A peer from resolve_peer carries no channel flags, so the forum bit has
    // to come from the fetched channel.
    let mut forum = false;
    let (about, members) = match peer_ref.id.kind() {
        PeerKind::User => {
            let request = tl::functions::users::GetFullUser {
                id: (&peer_ref).into(),
            };
            let tl::enums::users::UserFull::Full(full) = handles.client.invoke(&request).await?;
            let tl::enums::UserFull::Full(user) = full.full_user;
            extra.muted = is_muted(&user.notify_settings);
            extra.common_chats = (user.common_chats_count > 0).then_some(user.common_chats_count);
            extra.birthday = user.birthday.map(|tl::enums::Birthday::Birthday(day)| match day.year {
                Some(year) => format!("{:02}.{:02}.{year}", day.day, day.month),
                None => format!("{:02}.{:02}", day.day, day.month),
            });
            (user.about.unwrap_or_default(), None)
        }
        PeerKind::Chat => {
            let request = tl::functions::messages::GetFullChat {
                chat_id: peer_ref.id.bare_id().unwrap_or_default(),
            };
            let tl::enums::messages::ChatFull::Full(full) = handles.client.invoke(&request).await?;
            match full.full_chat {
                ChatFull::Full(chat) => {
                    extra.muted = is_muted(&chat.notify_settings);
                    extra.invite_link = invite_link(chat.exported_invite);
                    let members = match chat.participants {
                        tl::enums::ChatParticipants::Participants(list) => {
                            Some(list.participants.len() as i32)
                        }
                        _ => None,
                    };
                    (chat.about, members)
                }
                ChatFull::ChannelFull(channel) => (channel.about, channel.participants_count),
                _ => (String::new(), None),
            }
        }
        PeerKind::Channel => {
            let request = tl::functions::channels::GetFullChannel {
                channel: (&peer_ref).into(),
            };
            let tl::enums::messages::ChatFull::Full(full) = handles.client.invoke(&request).await?;
            match full.full_chat {
                ChatFull::ChannelFull(channel) => {
                    extra.muted = is_muted(&channel.notify_settings);
                    extra.online = channel.online_count.filter(|count| *count > 0);
                    let request = tl::functions::channels::GetChannels {
                        id: vec![(&peer_ref).into()],
                    };
                    if let Ok(tl::enums::messages::Chats::Chats(found)) =
                        handles.client.invoke(&request).await
                    {
                        forum = found.chats.iter().any(
                            |chat| matches!(chat, tl::enums::Chat::Channel(channel) if channel.forum),
                        );
                    }
                    extra.invite_link = invite_link(channel.exported_invite);
                    (channel.about, channel.participants_count)
                }
                ChatFull::Full(chat) => (chat.about, None),
                _ => (String::new(), None),
            }
        }
    };

    // Counters are a nicety: a failure leaves the sections out.
    let counters = tl::functions::messages::GetSearchCounters {
        peer: (&peer_ref).into(),
        saved_peer_id: None,
        top_msg_id: None,
        filters: MEDIA_FILTERS.iter().map(|(_, filter)| filter.clone()).collect(),
    };
    let media = match handles.client.invoke(&counters).await {
        Ok(found) => MEDIA_FILTERS
            .iter()
            .zip(found)
            .filter_map(|((kind, _), tl::enums::messages::SearchCounter::Counter(counter))| {
                (counter.count > 0).then_some(MediaCount {
                    kind,
                    count: counter.count,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    Ok(PeerInfo {
        id: peer_id,
        kind: summary.kind,
        title: summary.title,
        photo_id: summary.photo_id,
        username: peer.username().map(str::to_owned),
        phone,
        about,
        members,
        online: extra.online,
        birthday: extra.birthday,
        common_chats: extra.common_chats,
        invite_link: extra.invite_link,
        muted: extra.muted,
        forum: forum || matches!(&peer, Peer::Channel(channel) if channel.raw.forum),
        media,
    })
}

/// Profile details that only some kinds of chat have.
#[derive(Default)]
struct Extra {
    online: Option<i32>,
    birthday: Option<String>,
    common_chats: Option<i32>,
    invite_link: Option<String>,
    muted: bool,
}

/// Basic groups that were upgraded to a supergroup, deleted, or left stay in
/// the dialog list forever with only a service message in them.
fn is_defunct(dialog: &Dialog) -> bool {
    match dialog.peer() {
        Peer::Group(group) => match &group.raw {
            tl::enums::Chat::Chat(chat) => {
                chat.deactivated || chat.left || chat.migrated_to.is_some()
            }
            tl::enums::Chat::Forbidden(_) | tl::enums::Chat::ChannelForbidden(_) => true,
            _ => false,
        },
        _ => false,
    }
}

fn no_picture() -> UiError {
    UiError::new("MEDIA_EMPTY", "the picture is not available")
}

fn path_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

/// Turns a dialog ID from the frontend back into something requests can
/// address. Works for every peer the session has seen, i.e. every dialog
/// that was listed.
pub(crate) async fn resolve_peer(handles: &Handles, peer_id: i64) -> Result<PeerRef, UiError> {
    let unknown = || UiError::new("PEER_UNKNOWN", "unknown chat");
    let id = PeerId::from_bot_api_dialog_id(peer_id).ok_or_else(unknown)?;
    handles
        .session
        .peer_ref(id)
        .await
        .map_err(UiError::internal)?
        .ok_or_else(unknown)
}

pub(crate) fn with_db<T>(
    db: &Db,
    run: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T>,
) -> Result<T, UiError> {
    let mut conn = db
        .lock()
        .map_err(|_| UiError::internal("database lock is poisoned"))?;
    Ok(run(&mut conn)?)
}

/// A user's first and last name (as saved in contacts, when they are one),
/// or a group's or channel's title.
pub fn display_name(peer: &Peer) -> String {
    match peer {
        Peer::User(user) => user.full_name(),
        other => other.name().unwrap_or_default().to_owned(),
    }
}

pub fn peer_summary(peer: Option<&Peer>) -> PeerSummary {
    let (kind, photo_id) = match peer {
        // The chat with oneself is "Saved Messages"; the frontend names it.
        Some(Peer::User(user)) if user.is_self() => ("saved", None),
        Some(Peer::User(user)) => ("user", user.photo().map(|photo| photo.photo_id)),
        Some(Peer::Group(group)) => ("group", group.photo().map(|photo| photo.photo_id)),
        Some(Peer::Channel(channel)) => ("channel", channel.photo().map(|photo| photo.photo_id)),
        _ => ("group", None),
    };
    PeerSummary {
        kind: kind.to_owned(),
        title: match (kind, peer) {
            ("saved", _) | (_, None) => String::new(),
            (_, Some(peer)) => display_name(peer),
        },
        photo_id: photo_id.map(|id| id.to_string()),
    }
}

fn dialog_row(dialog: &Dialog) -> DialogRow {
    let (unread, pinned, archived, read_outbox, unread_reactions, muted) = match &dialog.raw {
        tl::enums::Dialog::Dialog(raw) => (
            raw.unread_count,
            raw.pinned,
            raw.folder_id == Some(ARCHIVE_FOLDER),
            raw.read_outbox_max_id,
            raw.unread_reactions_count,
            is_muted(&raw.notify_settings),
        ),
        _ => (0, false, false, 0, 0, false),
    };
    let (contact, bot) = match dialog.peer() {
        Peer::User(user) => match &user.raw {
            tl::enums::User::User(raw) => (raw.contact, raw.bot),
            _ => (false, false),
        },
        _ => (false, false),
    };
    let presence = match dialog.peer() {
        Peer::User(user) if !user.is_self() && !user.is_bot() => Some(presence(user.status())),
        _ => None,
    };
    let can_post = match dialog.peer() {
        Peer::Channel(channel) if !channel.raw.megagroup => {
            channel.admin_rights().is_some_and(|rights| rights.post_messages)
        }
        _ => true,
    };
    let summary = peer_summary(Some(dialog.peer()));
    let last = dialog.last_message.as_ref();
    DialogRow {
        id: dialog_id(dialog.peer_id()),
        kind: summary.kind,
        title: summary.title,
        last_text: last.map(preview).unwrap_or_default(),
        last_date: last.map(|m| m.date().as_second()).unwrap_or_default(),
        last_outgoing: last.is_some_and(Message::outgoing),
        last_id: last.map(Message::id).unwrap_or_default(),
        unread,
        pinned,
        archived,
        photo_id: summary.photo_id,
        read_outbox,
        unread_reactions,
        muted,
        contact,
        bot,
        presence,
        can_post,
    }
}

/// A chat folder, with the rules official clients use to fill it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: i32,
    pub title: String,
    pub emoticon: Option<String>,
    pub contacts: bool,
    pub non_contacts: bool,
    pub groups: bool,
    pub channels: bool,
    pub bots: bool,
    pub exclude_muted: bool,
    pub exclude_read: bool,
    pub exclude_archived: bool,
    /// Chats always in the folder, pinned ones first.
    pub include: Vec<i64>,
    pub exclude: Vec<i64>,
}

/// The dialog ID of a peer as a folder names it.
fn input_peer_id(peer: &tl::enums::InputPeer, self_id: i64) -> Option<i64> {
    use tl::enums::InputPeer as I;
    let id = match peer {
        I::PeerSelf => return Some(self_id),
        I::User(user) => PeerId::user(user.user_id)?,
        I::UserFromMessage(user) => PeerId::user(user.user_id)?,
        I::Chat(chat) => PeerId::chat(chat.chat_id)?,
        I::Channel(channel) => PeerId::channel(channel.channel_id)?,
        I::ChannelFromMessage(channel) => PeerId::channel(channel.channel_id)?,
        I::Empty => return None,
    };
    Some(dialog_id(id))
}

/// The user's chat folders, in their order.
pub async fn folders(tg: &Telegram, self_id: i64) -> Result<Vec<Folder>, UiError> {
    let handles = tg.handles().await?;
    let tl::enums::messages::DialogFilters::Filters(found) = handles
        .client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await?;
    let ids = |peers: &[tl::enums::InputPeer]| -> Vec<i64> {
        peers.iter().filter_map(|peer| input_peer_id(peer, self_id)).collect()
    };
    Ok(found
        .filters
        .into_iter()
        .filter_map(|filter| match filter {
            tl::enums::DialogFilter::Filter(f) => {
                let tl::enums::TextWithEntities::Entities(title) = f.title;
                let mut include = ids(&f.pinned_peers);
                include.extend(ids(&f.include_peers));
                Some(Folder {
                    id: f.id,
                    title: title.text,
                    emoticon: f.emoticon,
                    contacts: f.contacts,
                    non_contacts: f.non_contacts,
                    groups: f.groups,
                    channels: f.broadcasts,
                    bots: f.bots,
                    exclude_muted: f.exclude_muted,
                    exclude_read: f.exclude_read,
                    exclude_archived: f.exclude_archived,
                    include,
                    exclude: ids(&f.exclude_peers),
                })
            }
            tl::enums::DialogFilter::Chatlist(f) => {
                let tl::enums::TextWithEntities::Entities(title) = f.title;
                let mut include = ids(&f.pinned_peers);
                include.extend(ids(&f.include_peers));
                Some(Folder {
                    id: f.id,
                    title: title.text,
                    emoticon: f.emoticon,
                    contacts: false,
                    non_contacts: false,
                    groups: false,
                    channels: false,
                    bots: false,
                    exclude_muted: false,
                    exclude_read: false,
                    exclude_archived: false,
                    include,
                    exclude: Vec::new(),
                })
            }
            tl::enums::DialogFilter::Default => None,
        })
        .collect())
}

/// Formatting spans the window knows how to draw; the rest are dropped.
fn entities(list: &[tl::enums::MessageEntity]) -> Vec<cache::Entity> {
    use tl::enums::MessageEntity as E;
    let span = |kind: &str, offset: i32, length: i32, target: Option<String>| cache::Entity {
        kind: kind.to_owned(),
        offset,
        length,
        target,
    };
    list.iter()
        .filter_map(|entity| {
            Some(match entity {
                E::Bold(e) => span("bold", e.offset, e.length, None),
                E::Italic(e) => span("italic", e.offset, e.length, None),
                E::Underline(e) => span("underline", e.offset, e.length, None),
                E::Strike(e) => span("strike", e.offset, e.length, None),
                E::Code(e) => span("code", e.offset, e.length, None),
                E::Pre(e) => span("pre", e.offset, e.length, None),
                E::Spoiler(e) => span("spoiler", e.offset, e.length, None),
                E::Blockquote(e) => span("quote", e.offset, e.length, None),
                E::Url(e) => span("url", e.offset, e.length, None),
                E::TextUrl(e) => span("textUrl", e.offset, e.length, Some(e.url.clone())),
                E::Mention(e) => span("mention", e.offset, e.length, None),
                E::MentionName(e) => {
                    let id = PeerId::user(e.user_id).map(dialog_id)?;
                    span("mentionName", e.offset, e.length, Some(id.to_string()))
                }
                E::Hashtag(e) => span("hashtag", e.offset, e.length, None),
                E::Email(e) => span("email", e.offset, e.length, None),
                E::Phone(e) => span("phone", e.offset, e.length, None),
                E::BotCommand(e) => span("command", e.offset, e.length, None),
                _ => return None,
            })
        })
        .collect()
}

fn web_preview(page: &tl::enums::WebPage) -> Option<cache::WebPreview> {
    let tl::enums::WebPage::Page(page) = page else {
        return None;
    };
    let preview = cache::WebPreview {
        url: page.url.clone(),
        site: page.site_name.clone().unwrap_or_default(),
        title: page.title.clone().unwrap_or_default(),
        description: page
            .description
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect(),
    };
    // A preview with nothing to say is not worth a box.
    (!preview.site.is_empty() || !preview.title.is_empty() || !preview.description.is_empty())
        .then_some(preview)
}

/// The reactions under a message, most popular first (Telegram's order).
pub fn reactions(raw: Option<&tl::enums::MessageReactions>) -> Vec<cache::Reaction> {
    let Some(tl::enums::MessageReactions::Reactions(raw)) = raw else {
        return Vec::new();
    };
    let mut list: Vec<cache::Reaction> = Vec::new();
    for tl::enums::ReactionCount::Count(count) in &raw.results {
        let emoji = match &count.reaction {
            tl::enums::Reaction::Emoji(emoji) => emoji.emoticon.clone(),
            // A custom emoji is named by its document; the window draws its
            // picture when that is switched on, a sparkle otherwise.
            tl::enums::Reaction::CustomEmoji(custom) => format!("custom:{}", custom.document_id),
            // Telegram Stars: shown, never sent from here (they cost money).
            tl::enums::Reaction::Paid => "paid".to_owned(),
            tl::enums::Reaction::Empty => continue,
        };
        let mine = count.chosen_order.is_some();
        // One entry per emoji: the window keys the list by it.
        match list.iter_mut().find(|reaction| reaction.emoji == emoji) {
            Some(same) => {
                same.count += count.count;
                same.mine |= mine;
            }
            None => list.push(cache::Reaction { emoji, count: count.count, mine }),
        }
    }
    list
}

/// Turns Telegram's user status into what the window shows.
pub fn presence(status: &tl::enums::UserStatus) -> Presence {
    use tl::enums::UserStatus;
    let (kind, at) = match status {
        UserStatus::Online(online) => ("online", online.expires),
        UserStatus::Offline(offline) => ("offline", offline.was_online),
        UserStatus::Recently(_) => ("recently", 0),
        UserStatus::LastWeek(_) => ("week", 0),
        UserStatus::LastMonth(_) => ("month", 0),
        UserStatus::Empty => ("hidden", 0),
    };
    Presence {
        kind,
        at: at.into(),
    }
}

/// The current status of a private chat's user, fetched fresh when the chat
/// is opened (the list's copy may be minutes old).
pub async fn peer_presence(tg: &Telegram, peer_id: i64) -> Result<Option<Presence>, UiError> {
    let handles = tg.handles().await?;
    let peer_ref = resolve_peer(&handles, peer_id).await?;
    if peer_ref.id.kind() != PeerKind::User {
        return Ok(None);
    }
    let request = tl::functions::users::GetUsers {
        id: vec![(&peer_ref).into()],
    };
    let users = handles.client.invoke(&request).await?;
    Ok(users.into_iter().find_map(|user| match user {
        tl::enums::User::User(user) if !user.is_self && !user.bot => {
            Some(presence(user.status.as_ref().unwrap_or(&tl::enums::UserStatus::Empty)))
        }
        _ => None,
    }))
}

pub fn message_row(message: &Message, peer_id: i64) -> MessageRow {
    let info = media::describe(message);
    let (image_w, image_h) = info
        .image
        .as_ref()
        .map_or((0, 0), |image| (image.width, image.height));
    MessageRow {
        id: message.id(),
        peer_id,
        // Short updates carry no sender; an ID of 0 means "unknown".
        sender_id: message.sender_id().map(dialog_id).filter(|id| *id != 0),
        sender_name: message.sender().map(display_name).unwrap_or_default(),
        date: message.date().as_second(),
        text: service::text(message).unwrap_or_else(|| message.text().to_owned()),
        kind: info.kind.to_owned(),
        outgoing: message.outgoing(),
        // Telegram also stamps an edit date for changes it marks hidden
        // (reactions, for one); official clients do not say "edited" then.
        edited: message.edit_date().is_some() && !message.edit_hide(),
        image_w,
        image_h,
        file_name: info.file_name,
        file_size: info.file_size,
        playable: info.file_ext.is_some(),
        duration: info.duration,
        waveform: info.waveform,
        track: info.track,
        buttons: buttons(message),
        grouped_id: message.grouped_id().map(|id| id.to_string()),
        reply_to_id: message.reply_to_message_id(),
        topic_id: topic_id(message),
        entities: if message.action().is_some() {
            Vec::new()
        } else {
            message.fmt_entities().map(|list| entities(list)).unwrap_or_default()
        },
        web: match message.media() {
            Some(grammers_client::media::Media::WebPage(page)) => web_preview(&page.raw.webpage),
            _ => None,
        },
        reactions: match &message.raw {
            tl::enums::Message::Message(raw) => reactions(raw.reactions.as_ref()),
            _ => Vec::new(),
        },
        // A channel post with a discussion group: the comments under it, which
        // the client opens with `messages.getDiscussionMessage`.
        comments: match &message.raw {
            tl::enums::Message::Message(raw) => match &raw.replies {
                Some(tl::enums::MessageReplies::Replies(replies)) if replies.comments => replies.replies,
                _ => 0,
            },
            _ => 0,
        },
    }
}

/// The forum topic a message belongs to, from its reply header.
fn topic_id(message: &Message) -> Option<i32> {
    match &message.raw {
        tl::enums::Message::Message(raw) => match &raw.reply_to {
            Some(tl::enums::MessageReplyHeader::Header(header)) if header.forum_topic => {
                header.reply_to_top_id.or(header.reply_to_msg_id)
            }
            _ => None,
        },
        _ => None,
    }
}

/// A random ID for `messages.sendMessage`, unique enough for one client.
fn random_id() -> i64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or_default();
    nanos ^ (std::process::id() as i64) << 40
}

/// The ID of the message a sent update carries.
fn sent_id(updates: &tl::enums::Updates) -> Option<i32> {
    fn of_update(update: &tl::enums::Update) -> Option<i32> {
        let message = match update {
            tl::enums::Update::NewMessage(update) => &update.message,
            tl::enums::Update::NewChannelMessage(update) => &update.message,
            _ => return None,
        };
        match message {
            tl::enums::Message::Message(message) => Some(message.id),
            _ => None,
        }
    }
    match updates {
        tl::enums::Updates::UpdateShortSentMessage(update) => Some(update.id),
        tl::enums::Updates::UpdateShort(update) => of_update(&update.update),
        tl::enums::Updates::Updates(update) => update.updates.iter().find_map(of_update),
        tl::enums::Updates::Combined(update) => update.updates.iter().find_map(of_update),
        tl::enums::Updates::TooLong => None,
        tl::enums::Updates::UpdateShortMessage(_) | tl::enums::Updates::UpdateShortChatMessage(_) => None,
    }
}

pub fn dialog_id(id: PeerId) -> i64 {
    id.bot_api_dialog_id().unwrap_or_default()
}

/// The inline keyboard of a message, row by row.
fn buttons(message: &Message) -> Vec<Vec<cache::InlineButton>> {
    use tl::enums::InlineButtonType as T;
    let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = message.reply_markup() else {
        return Vec::new();
    };
    markup
        .rows
        .into_iter()
        .map(|row| {
            let tl::enums::KeyboardInlineButtonRow::Row(row) = row;
            row.buttons
                .into_iter()
                .map(|button| {
                    let tl::enums::KeyboardInlineButton::Button(button) = button;
                    let (kind, value) = match button.r#type {
                        T::Url(t) => ("url", t.url),
                        T::UrlAuth(t) => ("url", t.url),
                        T::WebView(t) => ("url", t.url),
                        T::Callback(t) if !t.requires_password => {
                            ("callback", t.data.iter().map(|b| format!("{b:02x}")).collect())
                        }
                        T::Copy(t) => ("copy", t.copy_text),
                        T::UserProfile(t) => ("user", t.user_id.to_string()),
                        _ => ("other", String::new()),
                    };
                    cache::InlineButton { text: button.text, kind: kind.to_owned(), value }
                })
                .collect()
        })
        .collect()
}

/// One-line text for the dialog list; "[kind]" when only the kind describes it.
pub fn preview(message: &Message) -> String {
    const MAX_CHARS: usize = 120;
    let text = service::text(message).unwrap_or_else(|| message.text().to_owned());
    let mut line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_CHARS)
        .collect();
    if line.trim().is_empty() {
        line = format!("[{}]", media::describe(message).kind);
    }
    line
}

#[cfg(test)]
mod save_tests {
    use super::{file_name, unique_path};

    #[test]
    fn names_are_cleaned_or_made_up() {
        assert_eq!(file_name("a/b:c?.zip", "document", None, 7), "a_b_c_.zip");
        assert_eq!(file_name("  ", "photo", Some("jpg"), 7), "photo_7.jpg");
        assert_eq!(file_name("", "document", None, 9), "file_9.bin");
    }

    #[test]
    fn existing_files_are_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("oxigram-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), b"1").unwrap();
        std::fs::write(dir.join("x (1).txt"), b"1").unwrap();
        assert_eq!(unique_path(&dir, "x.txt"), dir.join("x (2).txt"));
        assert_eq!(unique_path(&dir, "y"), dir.join("y"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod cache_tests {
    use super::trim_cache;

    #[test]
    fn oldest_files_go_first_and_the_new_one_stays() {
        let dir = std::env::temp_dir().join(format!("oxigram-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(dir.join(name), [0u8; 10]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        trim_cache(&dir, 15, &dir.join("c"));
        assert!(!dir.join("a").exists());
        assert!(!dir.join("b").exists());
        assert!(dir.join("c").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
