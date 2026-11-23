//! Pictures inside messages: photo thumbnails, video and GIF preview frames,
//! static stickers and round videos, laid out like the WebView build (single
//! pictures keep their shape, albums form a grid), plus the full-window viewer.
//!
//! Memory: a thumbnail is fetched only when its row is created on screen,
//! decoded straight to the size it is drawn at, and kept in a small cache
//! with a byte budget. Leaving the chat drops them all.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use oxigram_core::chats;
use oxigram_core::store::chats::MessageRow;
use oxigram_core::tr;
use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

use crate::{App, AppWindow, MediaCell, format, run, tg, ui, with_app};

/// Decoded thumbnails kept at once. A screen shows a dozen at most; the rest
/// is the margin the list creates above and below.
const THUMB_BUDGET: usize = 24 * 1024 * 1024;
/// Tallest a single picture is drawn, in logical pixels.
const MAX_HEIGHT: f32 = 420.0;
/// Widest a single picture is drawn (the bubble may shrink it further).
const MAX_WIDTH: f32 = 420.0;
/// Narrow pictures are not drawn thinner than this (unless smaller already).
const MIN_WIDTH: f32 = 160.0;
const STICKER_SIDE: f32 = 168.0;
const ROUND_SIDE: f32 = 240.0;
/// How much wider than the natural layout a channel post can be drawn
/// (the widest bubble is 520 px): thumbnails are decoded for that size.
const WIDE_SCALE: f32 = 520.0 / 420.0;
/// Gap between album cells.
const GAP: f32 = 2.0;
/// Corner radius of pictures, as in the WebView build.
pub const RADIUS: f32 = 12.0;

// Cell states, mirrored in app.slint.
const WAITING: i32 = 0;
const LOADING: i32 = 1;
const READY: i32 = 2;
const FAILED: i32 = 3;
const ASK: i32 = 4;

/// Decoded thumbnails of the open chat and what is being fetched.
#[derive(Default)]
pub struct Thumbs {
    images: HashMap<i32, (Image, usize)>,
    /// Least recently used first.
    order: VecDeque<i32>,
    bytes: usize,
    loading: HashSet<i32>,
    failed: HashSet<i32>,
    /// Clicked "Показать" while automatic loading is off.
    asked: HashSet<i32>,
}

impl Thumbs {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Stores a picture and returns the IDs pushed out of the budget.
    fn insert(&mut self, id: i32, image: Image, bytes: usize) -> Vec<i32> {
        self.images.insert(id, (image, bytes));
        self.order.retain(|&known| known != id);
        self.order.push_back(id);
        self.bytes += bytes;
        let mut evicted = Vec::new();
        while self.bytes > THUMB_BUDGET && self.order.len() > 1 {
            let Some(old) = self.order.pop_front() else { break };
            if let Some((_, size)) = self.images.remove(&old) {
                self.bytes -= size;
            }
            evicted.push(old);
        }
        evicted
    }
}

/// The full-window viewer's state.
pub struct Viewer {
    /// A video playing in the viewer, and the timer that pulls its frames.
    player: Option<(Box<dyn crate::video::Player>, slint::Timer)>,
    pub id: i32,
    /// Bumped on every open and close, so late downloads are dropped.
    token: u64,
    /// The downloaded original, once there.
    file: Option<PathBuf>,
    /// Where tr!("Save") put it.
    saved: Option<PathBuf>,
}

static VIEWER_TOKEN: AtomicU64 = AtomicU64::new(0);

/// A message with a picture to draw.
pub fn has_picture(m: &MessageRow) -> bool {
    m.image_w > 0 && m.image_h > 0
}

/// Where a picture goes inside the media box, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellBox {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Rounded corners: top-left, top-right, bottom-left, bottom-right.
    pub corners: [bool; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub w: f32,
    pub h: f32,
    pub cells: Vec<CellBox>,
    pub sticker: bool,
    pub round: bool,
}

/// Lays out the pictures of one bubble: a single picture at its own shape
/// (capped), several as an album grid.
/// `wide`: a channel post, drawn at the full width of the column.
pub fn layout(parts: &[&MessageRow], wide: bool) -> Option<Layout> {
    let pictures: Vec<&MessageRow> = parts.iter().copied().filter(|m| has_picture(m)).collect();
    let first = *pictures.first()?;
    if pictures.len() == 1 {
        let (w0, h0) = (first.image_w as f32, first.image_h as f32);
        let (w, h, sticker, round) = match first.kind.as_str() {
            "sticker" => {
                let k = (STICKER_SIDE / w0).min(STICKER_SIDE / h0);
                (w0 * k, h0 * k, true, false)
            }
            "round" => (ROUND_SIDE, ROUND_SIDE, false, true),
            // Full width, at most square: a tall picture is cropped.
            _ if wide => (MAX_WIDTH, (MAX_WIDTH * h0 / w0).min(MAX_WIDTH).round(), false, false),
            _ => {
                let fitted = w0.min(MAX_HEIGHT * w0 / h0).min(MAX_WIDTH);
                let w = fitted.max(MIN_WIDTH.min(w0)).round();
                // A very tall picture is cropped rather than drawn as a sliver.
                let h = (w * h0 / w0).min(MAX_HEIGHT * 1.3).round();
                (w, h, false, false)
            }
        };
        let rounded = !sticker;
        return Some(Layout {
            w,
            h,
            cells: vec![CellBox { id: first.id, x: 0.0, y: 0.0, w, h, corners: [rounded; 4] }],
            sticker,
            round,
        });
    }

    let n = pictures.len();
    let columns = if n == 2 || n == 4 { 2 } else { 3 };
    let width: f32 = if columns == 2 && !wide { 380.0 } else { 420.0 };
    let side = ((width - GAP * (columns - 1) as f32) / columns as f32).floor();
    let rows = n.div_ceil(columns);
    let mut cells = Vec::with_capacity(n);
    for (i, picture) in pictures.iter().enumerate() {
        let row = i / columns;
        let in_row = if row + 1 == rows { n - row * columns } else { columns };
        let col = i % columns;
        // A short last row stretches over the whole width.
        let cell_w = (width - GAP * (in_row - 1) as f32) / in_row as f32;
        let x = col as f32 * (cell_w + GAP);
        let y = row as f32 * (side + GAP);
        let first_col = col == 0;
        let last_col = col + 1 == in_row;
        cells.push(CellBox {
            id: picture.id,
            x,
            y,
            w: cell_w,
            h: side,
            corners: [
                row == 0 && first_col,
                row == 0 && last_col,
                row + 1 == rows && first_col,
                row + 1 == rows && last_col,
            ],
        });
    }
    Some(Layout {
        w: width,
        h: rows as f32 * side + GAP * (rows - 1) as f32,
        cells,
        sticker: false,
        round: false,
    })
}

/// "1:05", "12:00:31".
pub fn duration(seconds: i32) -> String {
    let s = seconds.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl App {
    /// Rows of `messages[index]`'s album (just `index` for a lone message).
    pub fn album(&self, index: usize) -> Range<usize> {
        let Some(group) = self.messages.get(index).and_then(|m| m.grouped_id.as_ref()) else {
            return index..index + 1;
        };
        let same = |i: usize| self.messages[i].grouped_id.as_ref() == Some(group);
        let mut start = index;
        while start > 0 && same(start - 1) {
            start -= 1;
        }
        let mut end = index + 1;
        while end < self.messages.len() && same(end) {
            end += 1;
        }
        start..end
    }

    /// The cells of a bubble, with whatever of their pictures is ready.
    pub fn media_cells(&self, layout: &Layout) -> ModelRc<MediaCell> {
        let cells: Vec<MediaCell> = layout
            .cells
            .iter()
            .map(|cell| {
                let m = self.messages.iter().find(|m| m.id == cell.id);
                let kind = m.map_or("", |m| m.kind.as_str());
                let (image, state) = match self.thumbs.images.get(&cell.id) {
                    Some((image, _)) => (image.clone(), READY),
                    None if self.thumbs.failed.contains(&cell.id) => (Image::default(), FAILED),
                    None if self.thumbs.loading.contains(&cell.id) => (Image::default(), LOADING),
                    None if !self.settings.autoload_images && !self.thumbs.asked.contains(&cell.id) => {
                        (Image::default(), ASK)
                    }
                    None => (Image::default(), WAITING),
                };
                let badge = match (kind, m.map_or(0, |m| m.duration)) {
                    ("gif", _) => "GIF".to_owned(),
                    ("video" | "round", d) if d > 0 => duration(d),
                    _ => String::new(),
                };
                MediaCell {
                    id: cell.id,
                    x: cell.x,
                    y: cell.y,
                    w: cell.w,
                    h: cell.h,
                    image,
                    state,
                    badge: badge.into(),
                    play: matches!(kind, "video" | "gif" | "round") && state != ASK,
                    tl: cell.corners[0],
                    tr: cell.corners[1],
                    bl: cell.corners[2],
                    br: cell.corners[3],
                }
            })
            .collect();
        ModelRc::new(VecModel::from(cells))
    }

    /// Redraws the bubble that shows message `id`.
    fn refresh_message(&self, id: i32) {
        if let Some(index) = self.messages.iter().position(|m| m.id == id) {
            self.refresh_row(index);
        }
    }

    /// Rebuilds every row, e.g. after "Загружать картинки сами" changed.
    pub fn refresh_all_rows(&self) {
        for index in 0..self.messages.len() {
            self.refresh_row(index);
        }
    }
}

pub fn wire(window: &AppWindow) {
    // Cells ask from their `init`, which may run while a model is being
    // changed: the request waits for the next turn of the event loop.
    window.on_want_picture(|id| {
        slint::Timer::single_shot(Duration::ZERO, move || request(id));
    });
    window.on_open_picture(|id| {
        slint::Timer::single_shot(Duration::ZERO, move || open(id));
    });
    window.on_viewer_close(close_viewer);
    window.on_viewer_save(save);
    window.on_viewer_menu(|x, y| viewer_menu(x, y));
    window.on_viewer_toggle_play(|| {
        with_player(|player| if player.is_paused() { player.play() } else { player.pause() });
    });
    window.on_viewer_seek(|share| {
        with_player(|player| {
            let duration = player.duration();
            if duration > 0.0 {
                player.seek(duration * share.clamp(0.0, 1.0) as f64);
            }
        });
    });
    window.on_viewer_toggle_mute(|| {
        let Some(window) = ui() else { return };
        let muted = !window.get_viewer_muted();
        window.set_viewer_muted(muted);
        with_player(|player| player.set_muted(muted));
    });
}

/// Right click in the viewer: the same dark menu as elsewhere.
fn viewer_menu(x: f32, y: f32) {
    let Some(Some((id, has_file, saved, caption, video))) = with_app(|app| {
        let viewer = app.viewer.as_ref()?;
        let m = app.messages.iter().find(|m| m.id == viewer.id)?;
        Some((viewer.id, viewer.file.is_some(), viewer.saved.is_some(), !m.text.is_empty(), viewer.player.is_some()))
    }) else {
        return;
    };
    let mut items = Vec::new();
    if has_file {
        // A video copies the frame on screen.
        items.push(if video {
            crate::context_menu::entry("copy-frame", &tr!("Copy Frame"), crate::context_menu::COPY)
        } else {
            crate::context_menu::entry("copy-image", &tr!("Copy Image"), crate::context_menu::COPY)
        });
        items.push(if saved {
            crate::context_menu::entry("save", &tr!("Show in Folder"), crate::context_menu::SAVE)
        } else {
            crate::context_menu::entry("save", &tr!("Save"), crate::context_menu::SAVE)
        });
    }
    items.push(crate::context_menu::entry("forward", &tr!("Forward"), crate::context_menu::FORWARD));
    if caption {
        items.push(crate::context_menu::entry("copy", &tr!("Copy Caption"), crate::context_menu::COPY));
    }
    crate::context_menu::show(items, crate::context_menu::MenuTarget::Viewer(id), x, y);
}

pub fn viewer_action(id: i32, name: &str) {
    match name {
        "copy-frame" => {
            let frame = ui().map(|window| window.get_viewer_image()).and_then(|image| image.to_rgba8());
            if let Some(frame) = frame {
                copy_pixels(frame.width(), frame.height(), frame.as_bytes().to_vec());
            }
        }
        "copy-image" => {
            let file = with_app(|app| app.viewer.as_ref().and_then(|v| v.file.clone())).flatten();
            if let Some(file) = file {
                copy_image(file);
            }
        }
        "save" => save(),
        "forward" => {
            close_viewer();
            crate::messages::message_action(id, "forward".into());
        }
        "copy" => crate::messages::message_action(id, "copy".into()),
        _ => {}
    }
}

/// Puts RGBA pixels on the clipboard.
fn copy_pixels(width: u32, height: u32, rgba: Vec<u8>) {
    run(
        async move {
            tokio::task::spawn_blocking(move || {
                arboard::Clipboard::new()
                    .ok()?
                    .set_image(arboard::ImageData {
                        width: width as usize,
                        height: height as usize,
                        bytes: rgba.into(),
                    })
                    .ok()
            })
            .await
            .ok()
            .flatten()
        },
        |done| {
            crate::menus::toast(if done.is_some() { tr!("Frame copied") } else { tr!("Could not copy the frame") });
        },
    );
}

/// Puts the full picture on the clipboard.
pub fn copy_image(file: PathBuf) {
    run(
        async move {
            tokio::task::spawn_blocking(move || {
                let picture = crate::pictures::open_image(&file).ok()?.to_rgba8();
                let mut clipboard = arboard::Clipboard::new().ok()?;
                clipboard
                    .set_image(arboard::ImageData {
                        width: picture.width() as usize,
                        height: picture.height() as usize,
                        bytes: picture.into_raw().into(),
                    })
                    .ok()
            })
            .await
            .ok()
            .flatten()
        },
        |done| {
            crate::menus::toast(if done.is_some() {
                tr!("Image copied")
            } else {
                tr!("Could not copy the image")
            });
        },
    );
}

/// "Копировать картинку" in a message's menu: the original is fetched first.
pub fn copy_message_image(id: i32) {
    let Some(peer) = with_app(|app| app.chat).flatten() else { return };
    crate::menus::toast(tr!("Copying…"));
    run(async move { chats::message_file(tg(), peer, id, |_| {}).await }, |path| match path {
        Ok(path) => copy_image(PathBuf::from(path)),
        Err(_) => crate::menus::toast(tr!("Could not copy the image")),
    });
}

/// What to fetch for a cell: its size in device pixels and how to shape it.
struct Job {
    peer: i64,
    generation: u64,
    px_w: u32,
    px_h: u32,
    corners: [bool; 4],
    radius: f32,
    sticker: bool,
    round: bool,
}

fn request(id: i32) {
    let Some(Some(job)) = with_app(|app| {
        if app.thumbs.images.contains_key(&id)
            || app.thumbs.loading.contains(&id)
            || app.thumbs.failed.contains(&id)
        {
            return None;
        }
        if !app.settings.autoload_images && !app.thumbs.asked.contains(&id) {
            return None;
        }
        let peer = app.chat?;
        let index = app.messages.iter().position(|m| m.id == id)?;
        let range = app.album(index);
        let parts: Vec<&MessageRow> = app.messages[range].iter().collect();
        let wide = app.chat_kind() == "channel";
        let layout = layout(&parts, wide)?;
        let cell = layout.cells.iter().find(|c| c.id == id)?;
        let mut scale = app.ui.upgrade().map_or(1.0, |ui| ui.window().scale_factor());
        if wide && !layout.sticker && !layout.round {
            scale *= WIDE_SCALE;
        }
        app.thumbs.loading.insert(id);
        Some(Job {
            peer,
            generation: app.generation,
            px_w: (cell.w * scale).ceil().max(1.0) as u32,
            px_h: (cell.h * scale).ceil().max(1.0) as u32,
            corners: cell.corners,
            radius: RADIUS * scale,
            sticker: layout.sticker,
            round: layout.round,
        })
    }) else {
        return;
    };
    let generation = job.generation;
    run(
        async move {
            let path = chats::message_image(tg(), job.peer, id).await.ok()?;
            tokio::task::spawn_blocking(move || shape(Path::new(&path), &job)).await.ok().flatten()
        },
        move |pixels| {
            with_app(|app| {
                if app.generation != generation {
                    return;
                }
                app.thumbs.loading.remove(&id);
                match pixels {
                    Some(pixels) => {
                        let bytes = pixels.as_raw().len();
                        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            pixels.as_raw(),
                            pixels.width(),
                            pixels.height(),
                        );
                        let evicted = app.thumbs.insert(id, Image::from_rgba8(buffer), bytes);
                        for old in evicted {
                            app.refresh_message(old);
                        }
                    }
                    None => {
                        app.thumbs.failed.insert(id);
                    }
                }
                app.refresh_message(id);
            });
        },
    );
}

/// Decodes a downloaded picture straight to its drawn size and bakes the
/// rounded corners into the alpha channel (the software renderer does not
/// clip images to rounded rectangles).
fn shape(path: &Path, job: &Job) -> Option<image::RgbaImage> {
    let picture = crate::pictures::open_image(path).ok()?;
    let filter = image::imageops::FilterType::Triangle;
    if job.sticker {
        // Keep the whole sticker, centred in its box.
        let small = picture.resize(job.px_w, job.px_h, filter).to_rgba8();
        let mut canvas = image::RgbaImage::new(job.px_w, job.px_h);
        let x = (job.px_w.saturating_sub(small.width())) / 2;
        let y = (job.px_h.saturating_sub(small.height())) / 2;
        image::imageops::overlay(&mut canvas, &small, x.into(), y.into());
        return Some(canvas);
    }
    let mut pixels = picture.resize_to_fill(job.px_w, job.px_h, filter).to_rgba8();
    if job.round {
        return Some(crate::circle(pixels));
    }
    round_corners(&mut pixels, job.radius, job.corners);
    Some(pixels)
}

/// Fades the chosen corners out to a quarter circle of `radius` pixels.
fn round_corners(pixels: &mut image::RgbaImage, radius: f32, corners: [bool; 4]) {
    let (w, h) = (pixels.width() as f32, pixels.height() as f32);
    let r = radius.min(w / 2.0).min(h / 2.0);
    if r <= 0.0 {
        return;
    }
    let reach = r.ceil() as u32;
    let centres = [(r, r), (w - r, r), (r, h - r), (w - r, h - r)];
    for (corner, &(cx, cy)) in centres.iter().enumerate() {
        if !corners[corner] {
            continue;
        }
        let xs = if corner % 2 == 0 { 0..reach } else { (pixels.width() - reach)..pixels.width() };
        let ys = if corner < 2 { 0..reach } else { (pixels.height() - reach)..pixels.height() };
        for y in ys.clone() {
            for x in xs.clone() {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                // Only the part beyond the centre, towards the corner, fades.
                let outward = (corner % 2 == 0 && dx < 0.0 || corner % 2 == 1 && dx > 0.0)
                    && (corner < 2 && dy < 0.0 || corner >= 2 && dy > 0.0);
                if !outward {
                    continue;
                }
                let coverage = (r - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
                let pixel = pixels.get_pixel_mut(x, y);
                pixel.0[3] = (pixel.0[3] as f32 * coverage).round() as u8;
            }
        }
    }
}

// ---- Viewer -------------------------------------------------------------

fn open(id: i32) {
    let Some(Some((peer, row))) = with_app(|app| {
        let peer = app.chat?;
        let row = app.messages.iter().find(|m| m.id == id)?.clone();
        Some((peer, row))
    }) else {
        return;
    };

    // Automatic loading off: the first click only loads the thumbnail.
    let asked = with_app(|app| {
        if app.settings.autoload_images || app.thumbs.asked.contains(&id) {
            return false;
        }
        app.thumbs.asked.insert(id);
        app.refresh_message(id);
        true
    })
    .unwrap_or(false);
    if asked {
        return;
    }

    match row.kind.as_str() {
        // A sticker is already shown whole.
        "sticker" => {}
        "video" | "gif" | "round" => {
            if row.playable {
                show(id, peer, row.file_size, Shown::Player);
            } else {
                crate::menus::toast(tr!("This video format is not supported yet"));
            }
        }
        "photo" | "document" if row.playable => show(id, peer, row.file_size, Shown::Original),
        // A file with a preview, or a self-destructing photo: the preview is
        // all there is to show.
        _ => show(id, peer, 0, Shown::Thumbnail),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Shown {
    /// The full photo, downloaded on open.
    Original,
    /// The cached preview only.
    Thumbnail,
    /// A video: downloaded, then handed to the system player.
    Player,
}

/// The picture box in `app.slint`: 28 px on each side, and the bottom taken
/// by the video controls or left as a margin; the picture starts 56 px below
/// the top edge, which only moves it down, it does not shrink it (logical px).
const VIEWER_SIDE: f32 = 28.0;
const VIEWER_BOTTOM: f32 = 84.0;
const VIEWER_BOTTOM_VIDEO: f32 = 128.0;

/// The size a picture is drawn at, in device pixels, from the window's size
/// in device pixels and its scale factor.
fn viewer_box(window: (u32, u32), scale: f32, video: bool) -> (u32, u32) {
    let bottom = if video { VIEWER_BOTTOM_VIDEO } else { VIEWER_BOTTOM };
    let width = (window.0 as f32 - 2.0 * VIEWER_SIDE * scale).round().max(1.0) as u32;
    let height = (window.1 as f32 - bottom * scale).round().max(1.0) as u32;
    (width, height)
}

fn show(id: i32, peer: i64, size: i64, shown: Shown) {
    let token = VIEWER_TOKEN.fetch_add(1, Ordering::Relaxed) + 1;
    let Some(window) = ui() else { return };
    // The box the picture is drawn in, in device pixels: decoding to exactly
    // this leaves the renderer nothing to rescale, which made it soft.
    let scale = window.window().scale_factor();
    let (max_w, max_h) = {
        let size = window.window().size();
        viewer_box((size.width, size.height), scale, shown == Shown::Player)
    };
    with_app(|app| {
        app.viewer = Some(Viewer { id, token, file: None, saved: None, player: None });
    });
    window.set_viewer_image(Image::default());
    window.set_viewer_has_image(false);
    window.set_viewer_image_width(0.0);
    window.set_viewer_image_height(0.0);
    window.set_viewer_status(tr!("Loading…").into());
    window.set_viewer_save_text(SharedString::new());
    window.set_viewer_video(false);
    window.set_viewer_open(true);
    window.invoke_focus_keys();
    drop(window);

    let progress_total = size.max(0) as u64;
    let last = Arc::new(AtomicU64::new(0));
    run(
        async move {
            let path = match shown {
                Shown::Thumbnail => chats::message_image(tg(), peer, id).await,
                Shown::Original | Shown::Player => {
                    chats::message_file(tg(), peer, id, move |bytes| {
                        // A few updates a second are plenty.
                        let step = (progress_total / 50).max(64 * 1024);
                        if bytes.saturating_sub(last.load(Ordering::Relaxed)) < step {
                            return;
                        }
                        last.store(bytes, Ordering::Relaxed);
                        let _ = slint::invoke_from_event_loop(move || {
                            if current(token) {
                                if let Some(ui) = ui() {
                                    ui.set_viewer_status(progress_text(bytes, progress_total).into());
                                }
                            }
                        });
                    })
                    .await
                }
            }
            .map(PathBuf::from)
            .map_err(|err| format::error(&err))?;
            if shown == Shown::Player {
                return Ok((path, None));
            }
            let (w, h) = (max_w, max_h);
            let decode = path.clone();
            let pixels = tokio::task::spawn_blocking(move || {
                let picture = crate::pictures::open_image(&decode).ok()?;
                // Never larger than the window: a 4K original costs 30 MB.
                let picture = if picture.width() > w || picture.height() > h {
                    picture.resize(w, h, image::imageops::FilterType::Triangle)
                } else {
                    picture
                };
                Some(picture.to_rgba8())
            })
            .await
            .ok()
            .flatten()
            .ok_or_else(|| tr!("Could not open the image").to_owned())?;
            Ok((path, Some(pixels)))
        },
        move |result: Result<(PathBuf, Option<image::RgbaImage>), String>| {
            if !current(token) {
                return;
            }
            let Some(window) = ui() else { return };
            match result {
                Ok((path, None)) => {
                    // Played here when the system has an engine for it,
                    // otherwise in the system player.
                    if !play_here(&path, max_w, max_h) {
                        open_external(&path);
                        close_viewer();
                    }
                }
                Ok((path, Some(pixels))) => {
                    let (pixels_w, pixels_h) = (pixels.width(), pixels.height());
                    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        pixels.as_raw(),
                        pixels_w,
                        pixels_h,
                    );
                    window.set_viewer_image(Image::from_rgba8(buffer));
                    // Logical px: the image is drawn at exactly this size, so
                    // a picture smaller than the window is not blown up (which
                    // made a 1276 px screenshot a pixelated mess on 1080p).
                    window.set_viewer_image_width(pixels_w as f32 / scale);
                    window.set_viewer_image_height(pixels_h as f32 / scale);
                    window.set_viewer_has_image(true);
                    window.set_viewer_status(SharedString::new());
                    if shown == Shown::Original {
                        window.set_viewer_save_text(tr!("Save").into());
                    }
                    with_app(|app| {
                        if let Some(viewer) = app.viewer.as_mut() {
                            viewer.file = Some(path);
                        }
                    });
                }
                Err(text) => window.set_viewer_status(text.into()),
            }
        },
    );
}

fn current(token: u64) -> bool {
    with_app(|app| app.viewer.as_ref().is_some_and(|v| v.token == token)).unwrap_or(false)
}

fn progress_text(done: u64, total: u64) -> String {
    if total > 0 {
        tr!("{} of {}", crate::menus::format_size(done), crate::menus::format_size(total))
    } else {
        tr!("Loading… {}", crate::menus::format_size(done))
    }
}

/// Starts `path` in the viewer. False when no engine can play it.
fn play_here(path: &Path, max_w: u32, max_h: u32) -> bool {
    let Some(Some(looped)) = with_app(|app| {
        let id = app.viewer.as_ref()?.id;
        Some(app.messages.iter().find(|m| m.id == id).is_some_and(|m| m.kind == "gif"))
    }) else {
        return false;
    };
    let options = crate::video::Options {
        autoplay: true,
        looped,
        muted: looped,
        max_width: max_w,
        max_height: max_h,
    };
    let player = match crate::video::open(path, options) {
        Ok(player) => player,
        Err(_) => return false,
    };
    let fallback = path.to_path_buf();
    let timer = slint::Timer::default();
    // About once per display frame; a frame is copied only when a new one
    // was decoded.
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(15), move || tick(&fallback));
    with_app(|app| {
        if let Some(viewer) = app.viewer.as_mut() {
            viewer.player = Some((player, timer));
            viewer.file = Some(path.to_path_buf());
        }
    });
    if let Some(window) = ui() {
        window.set_viewer_status(SharedString::new());
        window.set_viewer_video(true);
        window.set_viewer_save_text(tr!("Save").into());
        window.set_viewer_muted(looped);
    }
    true
}

/// Pulls the newest frame and the playback state into the viewer.
fn tick(path: &Path) {
    let state = with_app(|app| {
        let (player, _) = app.viewer.as_mut()?.player.as_mut()?;
        if let Some(error) = player.error() {
            return Some(Err(error));
        }
        let frame = player.frame();
        Some(Ok((frame, player.position(), player.duration(), player.is_paused())))
    })
    .flatten();
    let Some(window) = ui() else { return };
    match state {
        Some(Ok((frame, position, duration, paused))) => {
            if let Some(frame) = frame {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &frame.rgba,
                    frame.width,
                    frame.height,
                );
                window.set_viewer_image(Image::from_rgba8(buffer));
                window.set_viewer_has_image(true);
            }
            window.set_viewer_playing(!paused);
            window.set_viewer_progress(if duration > 0.0 { (position / duration) as f32 } else { 0.0 });
            window.set_viewer_time(format!("{} / {}", duration_text(position), duration_text(duration)).into());
        }
        // The system cannot decode it after all: hand it to the system player.
        Some(Err(_)) => {
            open_external(path);
            close_viewer();
        }
        None => {}
    }
}

fn duration_text(seconds: f64) -> String {
    duration(seconds.max(0.0).floor() as i32)
}

fn with_player(f: impl FnOnce(&mut Box<dyn crate::video::Player>)) {
    with_app(|app| {
        if let Some((player, _)) = app.viewer.as_mut().and_then(|v| v.player.as_mut()) {
            f(player);
        }
    });
}

pub fn close_viewer() {
    // Dropping the player stops the sound and frees the decoder.
    with_app(|app| app.viewer = None);
    if let Some(ui) = ui() {
        ui.set_viewer_open(false);
        ui.set_viewer_video(false);
        // The decoded original is the biggest thing the app holds: drop it.
        ui.set_viewer_image(Image::default());
        ui.set_viewer_has_image(false);
    }
}

/// tr!("Save") copies the original to Downloads; then the button shows it in
/// the folder.
fn save() {
    let Some(Some((file, saved, id))) = with_app(|app| {
        let viewer = app.viewer.as_ref()?;
        Some((viewer.file.clone()?, viewer.saved.clone(), viewer.id))
    }) else {
        return;
    };
    if let Some(saved) = saved {
        reveal(&saved);
        return;
    }
    let Some(dir) = dirs::download_dir() else {
        crate::menus::toast(tr!("There is no Downloads folder"));
        return;
    };
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_owned();
    let target = free_name(&dir, &format!("oxigram_{id}"), &ext);
    match std::fs::copy(&file, &target) {
        Ok(_) => {
            with_app(|app| {
                if let Some(viewer) = app.viewer.as_mut() {
                    viewer.saved = Some(target.clone());
                }
            });
            if let Some(ui) = ui() {
                ui.set_viewer_save_text(tr!("Show in Folder").into());
            }
        }
        Err(_) => crate::menus::toast(tr!("Could not save")),
    }
}

/// `dir/stem.ext`, or `dir/stem (2).ext` and so on when taken.
/// A picture from the media cache (opened first when the passcode sealed it).
pub fn open_image(path: impl AsRef<Path>) -> image::ImageResult<image::DynamicImage> {
    let bytes = oxigram_core::vault::read_media(path.as_ref()).map_err(image::ImageError::IoError)?;
    image::load_from_memory(&bytes)
}

fn free_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}).{ext}")))
        .find(|path| !path.exists())
        .unwrap_or(first)
}

fn open_external(path: &Path) {
    #[cfg(windows)]
    let result = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();
    if result.is_err() {
        crate::menus::toast(tr!("Could not open the player"));
    }
}

fn reveal(path: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(windows))]
    if let Some(dir) = path.parent() {
        open_external(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i32, kind: &str, w: i32, h: i32) -> MessageRow {
        MessageRow {
            id,
            peer_id: 1,
            sender_id: None,
            sender_name: String::new(),
            date: 0,
            text: String::new(),
            kind: kind.into(),
            outgoing: false,
            edited: false,
            image_w: w,
            image_h: h,
            file_name: String::new(),
            file_size: 0,
            playable: true,
            duration: 0,
            waveform: Vec::new(),
            track: String::new(),
            buttons: Vec::new(),
            comments: 0,
            grouped_id: None,
            reactions: Vec::new(),
            reply_to_id: None,
            topic_id: None,
            entities: Vec::new(),
            web: None,
        }
    }

    #[test]
    fn single_pictures_keep_their_shape_within_limits() {
        let wide = row(1, "photo", 800, 400);
        let l = layout(&[&wide], false).unwrap();
        assert_eq!((l.w, l.h), (420.0, 210.0));

        let tall = row(2, "photo", 600, 1200);
        let l = layout(&[&tall], false).unwrap();
        // In a channel: full width, cropped to a square.
        let l2 = layout(&[&tall], true).unwrap();
        assert_eq!((l2.w, l2.h), (420.0, 420.0));
        assert_eq!((l.w, l.h), (210.0, 420.0));

        // Very narrow: not thinner than 160, cropped in height.
        let sliver = row(3, "photo", 100, 2000);
        let l = layout(&[&sliver], false).unwrap();
        assert_eq!(l.w, 100.0);

        let sticker = row(4, "sticker", 512, 256);
        let l = layout(&[&sticker], true).unwrap();
        assert!(l.sticker && l.w == 168.0 && l.h == 84.0);
        assert_eq!(l.cells[0].corners, [false; 4]);
    }

    #[test]
    fn albums_fill_whole_rows() {
        let parts: Vec<MessageRow> = (1..=5).map(|i| row(i, "photo", 800, 600)).collect();
        let refs: Vec<&MessageRow> = parts.iter().collect();
        let l = layout(&refs, false).unwrap();
        assert_eq!(l.w, 420.0);
        assert_eq!(l.cells.len(), 5);
        // Three on top, two stretched below; only the outer corners round.
        assert_eq!(l.cells[3].x, 0.0);
        assert!((l.cells[4].x + l.cells[4].w - 420.0).abs() < 0.01);
        assert_eq!(l.cells[0].corners, [true, false, false, false]);
        assert_eq!(l.cells[2].corners, [false, true, false, false]);
        assert_eq!(l.cells[4].corners, [false, false, false, true]);

        let pair: Vec<MessageRow> = (1..=2).map(|i| row(i, "photo", 800, 600)).collect();
        let refs: Vec<&MessageRow> = pair.iter().collect();
        let l = layout(&refs, false).unwrap();
        assert_eq!(l.w, 380.0);
        assert_eq!(l.cells[1].corners, [false, true, false, true]);
    }

    #[test]
    fn durations_read_like_telegram() {
        assert_eq!(duration(65), "1:05");
        assert_eq!(duration(3601), "1:00:01");
    }

    #[test]
    fn the_picture_box_follows_the_viewer_margins() {
        // 1000x800 at 100%: 28 px on each side, 84 below for a picture.
        assert_eq!(viewer_box((1000, 800), 1.0, false), (944, 716));
        // A video keeps room for its controls: 128 below, that is all.
        assert_eq!(viewer_box((1000, 800), 1.0, true), (944, 672));
        // Retina: the margins are logical, the window is in device pixels.
        assert_eq!(viewer_box((2000, 1600), 2.0, false), (1888, 1432));
        // Never smaller than a pixel, however small the window.
        assert_eq!(viewer_box((10, 10), 2.0, true), (1, 1));
    }
}
