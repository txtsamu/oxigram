//! Sending files: the clip button (system file dialog), Ctrl+V of pictures and
//! copied files, and drag-and-drop all stage files in one send window; from
//! there they go out as in the WebView build (src/views/Chat.svelte): photos
//! and videos in albums of up to 10, everything else one by one, the caption
//! under the first message. Videos are probed with the native video player
//! (duration, size, a preview frame) so they are sent as real videos.
//!
//! State lives in a thread-local owned by the UI thread; the upload itself is
//! one tokio task that the cancel button aborts.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxigram_core::actions::{self, AlbumItem, OutgoingFile};
use oxigram_core::error::UiError;
use oxigram_core::{tr, trn};
use oxigram_core::store::chats::MessageRow;
use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::messages::Compose;
use crate::video::Player;
use crate::{AppWindow, RUNTIME, SendItem, format, run, tg, ui, with_app};

/// Telegram's limit of items in one album.
const ALBUM_MAX: usize = 10;
/// Larger pictures go as files, as in the WebView build.
const PHOTO_MAX: u64 = 10 * 1024 * 1024;
/// Side of the square preview in the send window, in device pixels.
const PREVIEW_PX: u32 = 96;
/// A video that gives no frame in this time is sent as a plain file.
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Clone)]
enum Source {
    Path(PathBuf),
    /// A picture from the clipboard, already encoded.
    Memory(Arc<Vec<u8>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Probe {
    No,
    Queued,
    Running,
    Done,
}

struct Staged {
    id: u64,
    source: Source,
    name: String,
    size: u64,
    /// A format Telegram takes as a compressed photo.
    picture: bool,
    thumb: Option<Image>,
    probe: Probe,
    /// Seconds, width, height.
    video: Option<(u64, i32, i32)>,
    video_thumb: Option<Vec<u8>>,
}

impl Staged {
    fn photo_ok(&self) -> bool {
        self.picture && self.size <= PHOTO_MAX
    }
}

struct Running {
    id: u64,
    player: Box<dyn Player>,
    frame: Option<crate::video::Frame>,
    since: Instant,
}

/// An upload in progress.
struct Active {
    abort: tokio::task::AbortHandle,
    tokens: Vec<String>,
    sizes: HashMap<String, u64>,
    progress: HashMap<String, u64>,
    total: u64,
    count: usize,
    finished: usize,
    name: String,
}

#[derive(Default)]
struct State {
    files: Vec<Staged>,
    next_id: u64,
    running: Option<Running>,
    probe_timer: Option<slint::Timer>,
    send: Option<Active>,
    /// Files of one drop arrive as separate events; they are batched.
    dropped: Vec<PathBuf>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub fn wire(ui: &AppWindow) {
    ui.on_attach(pick_files);
    ui.on_send_remove(remove);
    ui.on_send_go(go);
    ui.on_send_cancel(cancel);
    ui.on_send_abort(abort);
    ui.on_paste_files(paste);
}

// ---- Classification ------------------------------------------------------

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default()
}

fn is_picture(name: &str) -> bool {
    matches!(extension(name).as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp")
}

/// Formats the WebView build probes as video too.
fn is_video(name: &str) -> bool {
    matches!(extension(name).as_str(), "mp4" | "m4v" | "mov" | "webm")
}

fn count_text(n: usize) -> String {
    trn!("{n} file", "{n} files", n)
}

// ---- Adding files --------------------------------------------------------

/// Whether files may be staged right now; explains with a toast when not.
fn can_stage() -> bool {
    let open = with_app(|app| app.chat.is_some()).unwrap_or(false);
    let Some(ui) = ui() else { return false };
    if !open {
        crate::menus::toast(tr!("Open a chat to send files"));
        return false;
    }
    if !ui.get_chat_can_post() {
        crate::menus::toast(tr!("You can't write in this chat"));
        return false;
    }
    if ui.get_bar_mode() == "edit" {
        crate::menus::toast(tr!("A file can't be attached while editing"));
        return false;
    }
    true
}

/// Stages files from disk (folders and empty files are skipped).
pub(crate) fn add_paths(paths: Vec<PathBuf>) {
    if !can_stage() {
        return;
    }
    let mut added = Vec::new();
    for path in paths {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() || meta.len() == 0 {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| tr!("File"));
        added.push((Source::Path(path), name, meta.len()));
    }
    stage(added);
}

fn stage(added: Vec<(Source, String, u64)>) {
    if added.is_empty() {
        crate::menus::toast(tr!("Nothing to send: the files are empty or are folders"));
        return;
    }
    let mut thumbs = Vec::new();
    st(|s| {
        for (source, name, size) in added {
            let id = s.next_id;
            s.next_id += 1;
            let picture = is_picture(&name);
            let video = is_video(&name) && matches!(source, Source::Path(_));
            if picture {
                thumbs.push((id, source.clone()));
            }
            s.files.push(Staged {
                id,
                source,
                name,
                size,
                picture,
                thumb: None,
                probe: if video { Probe::Queued } else { Probe::No },
                video: None,
                video_thumb: None,
            });
        }
    });
    for (id, source) in thumbs {
        make_thumb(id, source);
    }
    refresh();
    start_probes();
}

fn decode_thumb(source: &Source) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = match source {
        Source::Path(path) => image::open(path).ok()?,
        Source::Memory(bytes) => image::load_from_memory(bytes).ok()?,
    };
    let small = img
        .resize_to_fill(PREVIEW_PX, PREVIEW_PX, image::imageops::FilterType::Triangle)
        .to_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(small.as_raw(), small.width(), small.height()))
}

fn make_thumb(id: u64, source: Source) {
    run(
        async move { tokio::task::spawn_blocking(move || decode_thumb(&source)).await.ok().flatten() },
        move |buffer| {
            let Some(buffer) = buffer else { return };
            let found = st(|s| match s.files.iter_mut().find(|f| f.id == id) {
                Some(file) => {
                    file.thumb = Some(Image::from_rgba8(buffer));
                    true
                }
                None => false,
            });
            if found {
                refresh();
            }
        },
    );
}

/// Shows the staged files in the send window (opens or closes it).
fn refresh() {
    let Some(ui) = ui() else { return };
    let (items, count, single_picture, has_pictures) = st(|s| {
        let items: Vec<SendItem> = s
            .files
            .iter()
            .map(|f| SendItem {
                name: f.name.as_str().into(),
                size: crate::menus::format_size(f.size).into(),
                thumb: f.thumb.clone().unwrap_or_default(),
                has_thumb: f.thumb.is_some(),
            })
            .collect();
        let single = s.files.len() == 1 && s.files[0].photo_ok();
        (items, s.files.len(), single, s.files.iter().any(Staged::photo_ok))
    });
    if count == 0 {
        ui.set_send_open(false);
        ui.set_send_items(ModelRc::default());
        return;
    }
    ui.set_send_items(ModelRc::from(Rc::new(VecModel::from(items))));
    ui.set_send_count(count as i32);
    ui.set_send_count_text(count_text(count).into());
    ui.set_send_single_picture(single_picture);
    ui.set_send_has_pictures(has_pictures);
    if !ui.get_send_open() {
        ui.set_send_compress(true);
        ui.set_send_open(true);
    }
}

fn remove(index: i32) {
    st(|s| {
        if let Ok(index) = usize::try_from(index)
            && index < s.files.len()
        {
            let file = s.files.remove(index);
            if s.running.as_ref().is_some_and(|r| r.id == file.id) {
                s.running = None;
            }
        }
    });
    refresh();
}

fn cancel() {
    st(|s| {
        s.files.clear();
        s.running = None;
        s.probe_timer = None;
    });
    refresh();
}

// ---- File dialog, clipboard, drop ---------------------------------------

fn pick_files() {
    if !can_stage() {
        return;
    }
    let mut dialog = rfd::AsyncFileDialog::new().set_title(tr!("Choose files"));
    if let Some(ui) = ui()
        && let Some(with_parent) = ui
            .window()
            .with_winit_window(|w| rfd::AsyncFileDialog::new().set_title(tr!("Choose files")).set_parent(w))
    {
        dialog = with_parent;
    }
    let Some(runtime) = RUNTIME.get() else { return };
    runtime.spawn(async move {
        let Some(files) = dialog.pick_files().await else { return };
        let paths: Vec<PathBuf> = files.into_iter().map(|f| f.path().to_path_buf()).collect();
        let _ = slint::invoke_from_event_loop(move || add_paths(paths));
    });
}

/// Ctrl+V: takes pictures and copied files from the clipboard. False leaves
/// the paste to the text field.
fn paste() -> bool {
    let open = with_app(|app| app.chat.is_some()).unwrap_or(false);
    if !open || ui().is_none_or(|ui| !ui.get_chat_can_post()) {
        return false;
    }
    let files = clipboard_files();
    if !files.is_empty() {
        add_paths(files);
        return true;
    }
    let Ok(mut clipboard) = arboard::Clipboard::new() else { return false };
    match clipboard.get_image() {
        Ok(image) => {
            let Some(rgba) = image::RgbaImage::from_raw(image.width as u32, image.height as u32, image.bytes.into_owned())
            else {
                return false;
            };
            let mut png = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut png,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            );
            if rgba.write_with_encoder(encoder).is_err() {
                return false;
            }
            let size = png.len() as u64;
            if can_stage() {
                stage(vec![(Source::Memory(Arc::new(png)), "image.png".to_owned(), size)]);
            }
            true
        }
        Err(_) => false,
    }
}

#[cfg(windows)]
fn clipboard_files() -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    /// CF_HDROP: a list of files copied in Explorer.
    const CF_HDROP: u32 = 15;
    let mut out = Vec::new();
    unsafe {
        if IsClipboardFormatAvailable(CF_HDROP).is_err() || OpenClipboard(None).is_err() {
            return out;
        }
        if let Ok(handle) = GetClipboardData(CF_HDROP) {
            let drop = HDROP(handle.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            for index in 0..count {
                let len = DragQueryFileW(drop, index, None) as usize;
                let mut buf = vec![0u16; len + 1];
                let got = DragQueryFileW(drop, index, Some(&mut buf)) as usize;
                out.push(PathBuf::from(std::ffi::OsString::from_wide(&buf[..got.min(len)])));
            }
        }
        let _ = CloseClipboard();
    }
    out
}

/// Elsewhere: best effort, a clipboard text that is a list of `file://` links.
#[cfg(not(windows))]
fn clipboard_files() -> Vec<PathBuf> {
    let Ok(mut clipboard) = arboard::Clipboard::new() else { return Vec::new() };
    let Ok(text) = clipboard.get_text() else { return Vec::new() };
    parse_uri_list(&text)
}

#[cfg(not(windows))]
fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some(rest) = line.strip_prefix("file://") else { return Vec::new() };
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        out.push(PathBuf::from(percent_decode(rest)));
    }
    out
}

#[cfg(not(windows))]
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len() + 0
            && let Ok(v) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// winit's `DroppedFile`, one event per file.
pub fn dropped(path: PathBuf) {
    let first = st(|s| {
        s.dropped.push(path);
        s.dropped.len() == 1
    });
    if let Some(ui) = ui() {
        ui.set_drop_hover(false);
    }
    if first {
        slint::Timer::single_shot(Duration::from_millis(60), || {
            let paths = st(|s| std::mem::take(&mut s.dropped));
            add_paths(paths);
        });
    }
}

/// winit's `HoveredFile` / `HoveredFileCancelled`.
pub fn hover(on: bool) {
    if let Some(ui) = ui() {
        ui.set_drop_hover(on);
    }
}

// ---- Videos --------------------------------------------------------------

fn start_probes() {
    let need = st(|s| s.files.iter().any(|f| matches!(f.probe, Probe::Queued | Probe::Running)));
    if !need || st(|s| s.probe_timer.is_some()) {
        return;
    }
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(40), probe_tick);
    st(|s| s.probe_timer = Some(timer));
}

/// Pixels of a first frame as a JPEG for Telegram and a small preview.
fn frame_images(frame: crate::video::Frame) -> Option<(Vec<u8>, SharedPixelBuffer<Rgba8Pixel>)> {
    let rgba = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)?;
    let img = image::DynamicImage::ImageRgba8(rgba);
    let rgb = img.thumbnail(320, 320).to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&rgb)
        .ok()?;
    let small = img
        .resize_to_fill(PREVIEW_PX, PREVIEW_PX, image::imageops::FilterType::Triangle)
        .to_rgba8();
    Some((jpeg, SharedPixelBuffer::clone_from_slice(small.as_raw(), small.width(), small.height())))
}

fn probe_tick() {
    let changed = st(|s| {
        let mut changed = false;
        if s.running.is_none()
            && let Some(index) = s.files.iter().position(|f| f.probe == Probe::Queued)
        {
            let Source::Path(path) = s.files[index].source.clone() else {
                s.files[index].probe = Probe::Done;
                return false;
            };
            // Muted and played just long enough to get the first frame; the
            // engine scales big videos down to 4096 px frames at most.
            let options = crate::video::Options {
                autoplay: true,
                looped: false,
                muted: true,
                max_width: 4096,
                max_height: 4096,
            };
            match crate::video::open(&path, options) {
                Ok(player) => {
                    s.files[index].probe = Probe::Running;
                    s.running = Some(Running { id: s.files[index].id, player, frame: None, since: Instant::now() });
                }
                Err(_) => {
                    s.files[index].probe = Probe::Done;
                    changed = true;
                }
            }
        }
        if let Some(run) = s.running.as_mut() {
            if let Some(frame) = run.player.frame() {
                run.frame = Some(frame);
            }
            let duration = run.player.duration();
            let failed = run.player.error().is_some() || run.since.elapsed() > PROBE_TIMEOUT;
            let ready = run.frame.is_some() && duration.is_finite() && duration > 0.0;
            if failed || ready {
                let id = run.id;
                let finished = s.running.take().expect("checked above");
                let mut found = None;
                if ready && !failed {
                    let frame = finished.frame.expect("ready");
                    let (w, h) = (frame.width as i32, frame.height as i32);
                    found = frame_images(frame).map(|images| ((duration.round().max(1.0) as u64, w, h), images));
                }
                if let Some(file) = s.files.iter_mut().find(|f| f.id == id) {
                    file.probe = Probe::Done;
                    if let Some((video, (jpeg, preview))) = found {
                        file.video = Some(video);
                        file.video_thumb = Some(jpeg);
                        file.thumb = Some(Image::from_rgba8(preview));
                    }
                }
                changed = true;
            }
        }
        if !s.files.iter().any(|f| matches!(f.probe, Probe::Queued | Probe::Running)) {
            s.probe_timer = None;
        }
        changed
    });
    if changed {
        refresh();
    }
}

// ---- Sending -------------------------------------------------------------

struct Job {
    token: String,
    source: Source,
    name: String,
    size: u64,
    as_photo: bool,
    video: Option<(u64, i32, i32)>,
    thumb: Option<Vec<u8>>,
}

fn go(caption: SharedString, compress: bool) {
    let caption = caption.to_string();
    if st(|s| s.send.is_some()) {
        crate::menus::toast(tr!("Wait for the sending to finish"));
        return;
    }
    if st(|s| s.files.is_empty()) {
        return;
    }
    if st(|s| s.files.iter().any(|f| matches!(f.probe, Probe::Queued | Probe::Running))) {
        crate::menus::toast(tr!("Reading the video, one moment…"));
        return;
    }
    if caption.encode_utf16().count() > 1024 {
        crate::menus::toast(tr!("The caption is longer than 1024 characters"));
        return;
    }
    let Some(Some((peer, compose, topic))) = with_app(|app| app.chat.map(|peer| (peer, app.compose, app.topic))) else {
        return;
    };
    let reply = match compose {
        Compose::Reply(id) => Some(id),
        _ => None,
    };
    let files = st(|s| std::mem::take(&mut s.files));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let jobs: Vec<Job> = files
        .into_iter()
        .enumerate()
        .map(|(index, f)| Job {
            token: format!("{stamp}-{index}-{}", f.id),
            as_photo: compress && f.photo_ok(),
            video: if compress && f.photo_ok() { None } else { f.video },
            thumb: f.video_thumb,
            source: f.source,
            name: f.name,
            size: f.size,
        })
        .collect();
    let total: u64 = jobs.iter().map(|j| j.size).sum();
    let count = jobs.len();
    let name = jobs[0].name.clone();
    let tokens: Vec<String> = jobs.iter().map(|j| j.token.clone()).collect();
    let sizes: HashMap<String, u64> = jobs.iter().map(|j| (j.token.clone(), j.size)).collect();

    // Photos and videos share albums, plain files form their own.
    let (visual, plain): (Vec<Job>, Vec<Job>) = jobs.into_iter().partition(|j| j.as_photo || j.video.is_some());
    let mut groups: Vec<Vec<Job>> = Vec::new();
    for list in [visual, plain] {
        let mut list = list.into_iter().peekable();
        while list.peek().is_some() {
            groups.push(list.by_ref().take(ALBUM_MAX).collect());
        }
    }

    if reply.is_some() {
        with_app(|app| app.compose = Compose::None);
    }
    if let Some(ui) = ui() {
        if reply.is_some() {
            ui.set_bar_mode(SharedString::new());
        }
        ui.set_send_open(false);
        ui.set_send_items(ModelRc::default());
        ui.set_sending_progress(0.0);
        ui.set_sending_active(true);
    }
    st(|s| s.running = None);

    let Some(runtime) = RUNTIME.get() else { return };
    let handle = runtime.spawn(send_all(peer, caption, reply, topic, groups));
    st(|s| {
        s.send = Some(Active {
            abort: handle.abort_handle(),
            tokens,
            sizes,
            progress: HashMap::new(),
            total,
            count,
            finished: 0,
            name,
        });
    });
    show_progress();
}

fn post(f: impl FnOnce() + Send + 'static) {
    let _ = slint::invoke_from_event_loop(f);
}

async fn send_all(peer: i64, caption: String, reply: Option<i32>, topic: Option<i32>, groups: Vec<Vec<Job>>) {
    for (index, group) in groups.into_iter().enumerate() {
        let text = if index == 0 { caption.clone() } else { String::new() };
        let reply_to = if index == 0 { reply } else { None };
        let tokens: Vec<String> = group.iter().map(|j| j.token.clone()).collect();
        match send_group(peer, group, text, reply_to, topic).await {
            Ok(rows) => post(move || {
                with_app(|app| {
                    for row in rows {
                        app.add_message(row);
                    }
                });
                group_done(&tokens);
            }),
            Err(err) => {
                let text = describe(&err);
                actions::discard_uploads(tg(), &tokens);
                post(move || {
                    crate::menus::toast(&text);
                    group_done(&tokens);
                });
            }
        }
    }
    post(finish);
}

async fn read(source: &Source) -> Result<Vec<u8>, UiError> {
    match source {
        Source::Memory(bytes) => Ok(bytes.as_ref().clone()),
        Source::Path(path) => {
            let path = path.clone();
            tokio::task::spawn_blocking(move || std::fs::read(&path))
                .await
                .map_err(|e| UiError::new("FILE_READ", e.to_string()))?
                .map_err(|e| UiError::new("FILE_READ", e.to_string()))
        }
    }
}

async fn send_group(
    peer: i64,
    group: Vec<Job>,
    caption: String,
    reply: Option<i32>,
    topic: Option<i32>,
) -> Result<Vec<MessageRow>, UiError> {
    if group.len() == 1 {
        let job = group.into_iter().next().expect("one job");
        let bytes = read(&job.source).await?;
        let file = OutgoingFile {
            peer_id: peer,
            name: job.name,
            caption,
            reply_to: reply.or(topic),
            topic,
            as_photo: job.as_photo,
            token: job.token,
            video: job.video,
        };
        return Ok(vec![actions::send_file_with_thumb(tg(), file, bytes, job.thumb).await?]);
    }
    let mut items = Vec::with_capacity(group.len());
    for job in &group {
        let bytes = read(&job.source).await?;
        actions::upload_file_with_thumb(tg(), &job.name, &job.token, bytes, job.thumb.clone()).await?;
        items.push(AlbumItem {
            token: job.token.clone(),
            as_photo: job.as_photo,
            video: job.video.map(|(s, w, h)| format!("{s},{w},{h}")).unwrap_or_default(),
        });
    }
    actions::send_album(tg(), peer, items, &caption, reply.or(topic)).await
}

/// Readable text for what can go wrong while sending a file.
fn describe(err: &UiError) -> String {
    match err.code.as_str() {
        "FILE_EMPTY" => tr!("The file is empty."),
        "FILE_TOO_BIG" => tr!("The file is too big: Oxigram sends files up to 512 MB."),
        "FILE_READ" => tr!("Could not read the file. It may have been deleted or be in use by another program."),
        "UPLOAD_FAILED" => tr!("Could not upload the file. Check your connection and try again."),
        "CAPTION_TOO_LONG" => tr!("The caption is longer than 1024 characters."),
        "PHOTO_INVALID_DIMENSIONS" | "IMAGE_PROCESS_FAILED" | "PHOTO_EXT_INVALID" | "PHOTO_SAVE_FILE_INVALID" => {
            tr!("Telegram did not accept the picture as a photo. Send it uncompressed, as a file.")
        }
        "MEDIA_EMPTY" | "FILE_PARTS_INVALID" => tr!("Telegram did not accept the file."),
        "CHAT_SEND_MEDIA_FORBIDDEN" | "CHAT_SEND_PHOTOS_FORBIDDEN" | "CHAT_SEND_VIDEOS_FORBIDDEN"
        | "CHAT_SEND_DOCS_FORBIDDEN" => tr!("You can't send files like this in this chat."),
        _ => format::error(err),
    }
}

// ---- Progress and cancel -------------------------------------------------

/// `UiEvent::Upload`: bytes of a file that went out so far.
pub fn progress(token: &str, bytes: u64) {
    let known = st(|s| match s.send.as_mut() {
        Some(active) if active.sizes.contains_key(token) => {
            active.progress.insert(token.to_owned(), bytes);
            true
        }
        _ => false,
    });
    if known {
        show_progress();
    }
}

fn group_done(tokens: &[String]) {
    st(|s| {
        if let Some(active) = s.send.as_mut() {
            for token in tokens {
                if let Some(size) = active.sizes.get(token) {
                    active.progress.insert(token.clone(), *size);
                }
            }
            active.finished += tokens.len();
        }
    });
    show_progress();
}

fn show_progress() {
    let Some(ui) = ui() else { return };
    let Some((text, ratio)) = st(|s| {
        let a = s.send.as_ref()?;
        let done: u64 = a
            .progress
            .iter()
            .map(|(t, b)| (*b).min(a.sizes.get(t).copied().unwrap_or(0)))
            .sum();
        let text = if a.count == 1 {
            tr!("Sending: {}", a.name)
        } else {
            tr!("Sending: {} of {}", (a.finished + 1).min(a.count), a.count)
        };
        Some((text, if a.total == 0 { 0.0 } else { done as f32 / a.total as f32 }))
    }) else {
        return;
    };
    ui.set_sending_text(text.into());
    ui.set_sending_progress(ratio);
}

fn finish() {
    st(|s| s.send = None);
    if let Some(ui) = ui() {
        ui.set_sending_active(false);
    }
}

/// The cross on the progress pill: stops the upload task.
fn abort() {
    let Some(active) = st(|s| s.send.take()) else { return };
    active.abort.abort();
    actions::discard_uploads(tg(), &active.tokens);
    if let Some(ui) = ui() {
        ui.set_sending_active(false);
    }
    crate::menus::toast(tr!("Sending cancelled"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension() {
        assert!(is_picture("Photo.JPG"));
        assert!(!is_picture("archive.zip"));
        assert!(is_video("clip.MP4"));
        assert!(!is_video("clip"));
    }

    #[test]
    fn count_text_is_plural() {
        assert_eq!(count_text(1), "1 file");
        assert_eq!(count_text(5), "5 files");
    }

    #[test]
    fn describes_errors() {
        let text = describe(&UiError::new("FILE_TOO_BIG", "the file is too big"));
        assert!(text.contains("512"));
        assert!(describe(&UiError::new("UPLOAD_FAILED", "x")).starts_with("Could not"));
    }

    #[cfg(not(windows))]
    #[test]
    fn uri_list() {
        let files = parse_uri_list("file:///home/a/%D1%84.txt\nfile:///b.png\n");
        assert_eq!(files, vec![PathBuf::from("/home/a/ф.txt"), PathBuf::from("/b.png")]);
        assert!(parse_uri_list("hello").is_empty());
    }

    #[test]
    fn album_groups_of_ten() {
        let mut list: Vec<u32> = (0..23).collect();
        let mut groups = Vec::new();
        let mut it = list.drain(..).peekable();
        while it.peek().is_some() {
            groups.push(it.by_ref().take(ALBUM_MAX).collect::<Vec<_>>());
        }
        assert_eq!(groups.iter().map(Vec::len).collect::<Vec<_>>(), vec![10, 10, 3]);
    }
}
