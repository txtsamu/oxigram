//! Voice messages and audio files played inside the window.
//!
//! * Decoding is pure Rust and needs no codec packs: `symphonia` reads
//!   MP3 / AAC / M4A / Vorbis / FLAC / WAV, and Ogg Opus (what Telegram voice
//!   is) goes through `opus-decoder` after symphonia's Ogg demuxer.
//! * Output is `cpal` (WASAPI, CoreAudio, ALSA). On Linux building it needs
//!   the ALSA headers (`libasound2-dev`); nothing else is required.
//! * One thread per playing track decodes in small steps into a queue of about
//!   half a second, so RAM stays flat however long the file is.
//! * Which track plays, and where, lives here (not in the rows, which Slint
//!   recreates while scrolling); the UI reads it through the `Player` global.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oxigram_core::chats;
use oxigram_core::tr;
use oxigram_core::store::chats::MessageRow;
use slint::ComponentHandle;

use crate::{Player, format, run, tg, ui, with_app};

// ---- What the bubble shows ---------------------------------------------------

/// Bars drawn for a voice message.
const BARS: usize = 40;
/// Size of the waveform in the bubble, in logical pixels (see `TrackRow`).
const WAVE_W: f32 = 180.0;
const WAVE_H: f32 = 26.0;

/// The fields of a message row that the player's row needs.
#[derive(Default)]
pub struct TrackItem {
    /// 0: not a playable track, 1: voice, 2: audio file.
    pub kind: i32,
    pub title: String,
    pub total: String,
    /// SVG path of the waveform; empty when the message has none.
    pub wave: String,
}

/// A voice message or audio file the player can open.
pub fn is_track(m: &MessageRow) -> bool {
    m.playable && matches!(m.kind.as_str(), "voice" | "audio")
}

pub fn item(m: &MessageRow) -> TrackItem {
    if !is_track(m) {
        return TrackItem::default();
    }
    let voice = m.kind == "voice";
    let title = if voice {
        String::new()
    } else if !m.track.is_empty() {
        m.track.clone()
    } else if !m.file_name.is_empty() {
        m.file_name.clone()
    } else {
        tr!("Audio")
    };
    TrackItem {
        kind: if voice { 1 } else { 2 },
        title,
        total: fmt_time(f64::from(m.duration)),
        wave: if voice { wave_path(&bars(&unpack_waveform(&m.waveform), BARS), WAVE_W, WAVE_H) } else { String::new() },
    }
}

/// Telegram's waveform: 5-bit values packed one after another, low bits first.
pub fn unpack_waveform(data: &[u8]) -> Vec<u8> {
    (0..data.len() * 8 / 5)
        .map(|i| {
            let bit = i * 5;
            let (byte, shift) = (bit / 8, bit % 8);
            let lo = u16::from(data[byte]);
            let hi = u16::from(data.get(byte + 1).copied().unwrap_or(0));
            (((hi << 8 | lo) >> shift) & 31) as u8
        })
        .collect()
}

/// Squeezes `values` (0..=31) into `count` bars of height 0..=1, scaled so the
/// loudest one is full and quiet voices are not drawn as dots.
pub fn bars(values: &[u8], count: usize) -> Vec<f32> {
    if values.is_empty() || count == 0 {
        return Vec::new();
    }
    let peak = f32::from(values.iter().copied().max().unwrap_or(0)).max(8.0);
    (0..count)
        .map(|i| {
            let lo = i * values.len() / count;
            let hi = ((i + 1) * values.len() / count).max(lo + 1).min(values.len());
            let slice = &values[lo..hi];
            let avg = slice.iter().map(|v| f32::from(*v)).sum::<f32>() / slice.len() as f32;
            (avg / peak).clamp(0.0, 1.0)
        })
        .collect()
}

/// Vertical bars as stroke segments of one SVG path, `w` x `h` pixels.
pub fn wave_path(bars: &[f32], w: f32, h: f32) -> String {
    if bars.is_empty() {
        return String::new();
    }
    let pitch = w / bars.len() as f32;
    let mut path = String::with_capacity(bars.len() * 20);
    for (i, bar) in bars.iter().enumerate() {
        let half = (bar * h / 2.0).max(1.5);
        let x = (i as f32 + 0.5) * pitch;
        path.push_str(&format!("M {x:.1} {:.1} L {x:.1} {:.1} ", h / 2.0 - half, h / 2.0 + half));
    }
    path
}

/// "0:07", "12:03", "1:02:03".
pub fn fmt_time(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds as u64 } else { 0 };
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Where a click on the bar lands, in seconds.
pub fn seek_target(fraction: f32, total: f64) -> f64 {
    f64::from(fraction.clamp(0.0, 1.0)) * total.max(0.0)
}

/// 0..=1 for the progress bar.
pub fn fraction(position: f64, total: f64) -> f32 {
    if total > 0.0 { (position / total).clamp(0.0, 1.0) as f32 } else { 0.0 }
}

// ---- Sample conversion ------------------------------------------------------

/// Re-maps interleaved frames from `from` channels to `to` channels.
fn remap_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    if from == 0 || to == 0 {
        return;
    }
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    for frame in input.chunks_exact(from) {
        match (from, to) {
            (1, _) => (0..to).for_each(|_| out.push(frame[0])),
            (_, 1) => out.push(frame.iter().sum::<f32>() / from as f32),
            _ => {
                for c in 0..to {
                    out.push(frame.get(c).copied().unwrap_or(0.0));
                }
            }
        }
    }
}

/// Streaming linear resampler for interleaved frames.
struct Resampler {
    /// Source frames per output frame.
    step: f64,
    channels: usize,
    /// Position of the next output frame inside `buf`, in source frames.
    pos: f64,
    buf: Vec<f32>,
}

impl Resampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        Self { step: f64::from(from) / f64::from(to.max(1)), channels, pos: 0.0, buf: Vec::new() }
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.buf.clear();
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < 1e-9 {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        self.buf.extend_from_slice(input);
        let frames = self.buf.len() / ch;
        loop {
            let i = self.pos as usize;
            if i + 1 >= frames {
                break;
            }
            let f = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let (a, b) = (self.buf[i * ch + c], self.buf[(i + 1) * ch + c]);
                out.push(a + (b - a) * f);
            }
            self.pos += self.step;
        }
        let drop = (self.pos as usize).min(frames);
        self.buf.drain(..drop * ch);
        self.pos -= drop as f64;
    }
}

// ---- Decoding ---------------------------------------------------------------

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::audio::well_known::CODEC_ID_OPUS;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

enum Dec {
    Symphonia(Box<dyn AudioDecoder>),
    Opus(opus_decoder::OpusDecoder),
}

struct Source {
    format: Box<dyn FormatReader>,
    dec: Dec,
    track_id: u32,
    rate: u32,
    channels: usize,
    scratch: Vec<f32>,
    path: PathBuf,
    /// The reader hit the end; some containers cannot seek after that.
    ended: bool,
}

/// An Opus frame is at most 120 ms: 5760 samples a channel at 48 kHz.
const OPUS_MAX_FRAME: usize = 5760;

impl Source {
    fn open(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| e.to_string())?;
        let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
        let track_id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("no audio parameters")?
            .clone();
        let channels = params.channels.as_ref().map_or(2, |c| c.count());
        let rate = params.sample_rate.unwrap_or(48_000);
        let dec = if params.codec == CODEC_ID_OPUS {
            if channels > 2 {
                return Err("multichannel Opus".into());
            }
            Dec::Opus(opus_decoder::OpusDecoder::new(48_000, channels).map_err(|e| e.to_string())?)
        } else {
            Dec::Symphonia(
                symphonia::default::get_codecs()
                    .make_audio_decoder(&params, &AudioDecoderOptions::default())
                    .map_err(|e| e.to_string())?,
            )
        };
        Ok(Self { format, dec, track_id, rate, channels, scratch: Vec::new(), path: path.to_owned(), ended: false })
    }

    fn seek(&mut self, seconds: f64) {
        if self.ended {
            if let Ok(fresh) = Source::open(&self.path) {
                *self = fresh;
            }
        }
        let time = Time::try_from_secs_f64(seconds.max(0.0)).unwrap_or(Time::ZERO);
        let _ = self.format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(self.track_id) });
        match &mut self.dec {
            Dec::Symphonia(d) => d.reset(),
            Dec::Opus(d) => d.reset(),
        }
    }

    /// Appends the next decoded packet (interleaved, `self.channels` wide at
    /// `self.rate`) to `out`. `Ok(false)` is the end of the file.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    self.ended = true;
                    return Ok(false);
                }
                Err(SymError::ResetRequired) => {
                    self.ended = true;
                    return Ok(false);
                }
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.ended = true;
                    return Ok(false);
                }
                Err(e) => return Err(e.to_string()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match &mut self.dec {
                Dec::Symphonia(d) => match d.decode(&packet) {
                    Ok(buf) => {
                        self.rate = buf.spec().rate();
                        self.channels = buf.spec().channels().count();
                        self.scratch.clear();
                        buf.copy_to_vec_interleaved(&mut self.scratch);
                        out.extend_from_slice(&self.scratch);
                        return Ok(true);
                    }
                    Err(SymError::DecodeError(_) | SymError::IoError(_)) => continue,
                    Err(e) => return Err(e.to_string()),
                },
                Dec::Opus(d) => {
                    self.scratch.resize(OPUS_MAX_FRAME * self.channels, 0.0);
                    match d.decode_float(&packet.data, &mut self.scratch, false) {
                        Ok(frames) => {
                            let ch = self.channels;
                            let skip = (packet.trim_start.get() as usize).min(frames);
                            let end = frames.saturating_sub(packet.trim_end.get() as usize).max(skip);
                            self.rate = 48_000;
                            out.extend_from_slice(&self.scratch[skip * ch..end * ch]);
                            return Ok(true);
                        }
                        // A damaged packet is a short gap, not the end.
                        Err(_) => continue,
                    }
                }
            }
        }
    }
}

// ---- Output -------------------------------------------------------------------

/// What the decoder thread and the audio callback share.
struct Shared {
    buf: Mutex<Buf>,
    paused: AtomicBool,
    stop: AtomicBool,
    finished: AtomicBool,
    failed: AtomicBool,
    /// Bumped by a seek so that a chunk decoded before it is dropped.
    epoch: AtomicU64,
    seek: Mutex<Option<f64>>,
}

struct Buf {
    queue: VecDeque<f32>,
    /// Frames the device has taken since `base`.
    played: u64,
    /// Position in the file (seconds) of frame 0 of `played`.
    base: f64,
    rate: u32,
    eof: bool,
    /// The device waits until enough is queued, so it never starts on a stutter.
    primed: bool,
}

pub struct Engine {
    shared: Arc<Shared>,
}

/// About half a second of audio waits in the queue.
const QUEUE_SECONDS: f64 = 0.5;
const PRIME_SECONDS: f64 = 0.15;

impl Engine {
    pub fn start(path: PathBuf, start: f64, paused: bool) -> Engine {
        let shared = Arc::new(Shared {
            buf: Mutex::new(Buf { queue: VecDeque::new(), played: 0, base: start, rate: 48_000, eof: false, primed: false }),
            paused: AtomicBool::new(paused),
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            seek: Mutex::new(if start > 0.0 { Some(start) } else { None }),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("audio".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                if worker_main(&path, &worker).is_err() {
                    worker.failed.store(true, Ordering::Release);
                }
            });
        if spawned.is_err() {
            shared.failed.store(true, Ordering::Release);
        }
        Engine { shared }
    }

    /// Seconds from the start of the file.
    pub fn position(&self) -> f64 {
        let b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        b.base + b.played as f64 / f64::from(b.rate.max(1))
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Release);
    }

    pub fn seek(&self, seconds: f64) {
        {
            let mut b = self.shared.buf.lock().unwrap_or_else(|e| e.into_inner());
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            b.queue.clear();
            b.played = 0;
            b.base = seconds;
            b.eof = false;
            b.primed = false;
        }
        self.shared.finished.store(false, Ordering::Release);
        *self.shared.seek.lock().unwrap_or_else(|e| e.into_inner()) = Some(seconds);
    }

    pub fn finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

fn worker_main(path: &Path, shared: &Arc<Shared>) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let mut source = Source::open(path)?;
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let out_rate = config.sample_rate().0;
    let out_channels = usize::from(config.channels());
    shared.buf.lock().unwrap_or_else(|e| e.into_inner()).rate = out_rate;

    let stream_config: cpal::StreamConfig = config.clone().into();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, &stream_config, shared, out_channels),
        cpal::SampleFormat::I16 => build::<i16>(&device, &stream_config, shared, out_channels),
        cpal::SampleFormat::U16 => build::<u16>(&device, &stream_config, shared, out_channels),
        cpal::SampleFormat::I32 => build::<i32>(&device, &stream_config, shared, out_channels),
        other => Err(format!("sample format {other:?}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    let mut stream_paused = false;

    let mut chunk: Vec<f32> = Vec::new();
    let mut mapped: Vec<f32> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();
    let mut resampler = Resampler::new(source.rate, out_rate, out_channels);
    let mut last_format = (source.rate, source.channels);
    let queue_cap = (f64::from(out_rate) * QUEUE_SECONDS) as usize * out_channels;
    let prime = (f64::from(out_rate) * PRIME_SECONDS) as usize * out_channels;
    let mut at_eof = false;

    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let paused = shared.paused.load(Ordering::Acquire);
        if paused != stream_paused {
            // Not every backend can pause; the callback outputs silence then.
            if paused {
                let _ = stream.pause();
            } else {
                let _ = stream.play();
            }
            stream_paused = paused;
        }
        let requested = shared.seek.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(seconds) = requested {
            source.seek(seconds);
            resampler.reset();
            at_eof = false;
        }
        let epoch = shared.epoch.load(Ordering::Acquire);
        let queued = {
            let mut b = shared.buf.lock().unwrap_or_else(|e| e.into_inner());
            if at_eof && !b.eof && b.epoch_matches(&shared.epoch, epoch) {
                b.eof = true;
                b.primed = true;
            }
            if b.eof && b.queue.is_empty() && !shared.finished.load(Ordering::Acquire) && at_eof {
                shared.finished.store(true, Ordering::Release);
            }
            b.queue.len()
        };
        if at_eof {
            if shared.finished.load(Ordering::Acquire) {
                // Let the device play out its last buffer, then idle until a
                // seek restarts decoding or the track is dropped.
                std::thread::sleep(Duration::from_millis(60));
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        if queued >= queue_cap || (paused && queued >= prime) {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        }

        chunk.clear();
        match source.read(&mut chunk) {
            Ok(true) => {}
            Ok(false) => {
                at_eof = true;
                continue;
            }
            Err(e) => return Err(e),
        }
        if (source.rate, source.channels) != last_format {
            last_format = (source.rate, source.channels);
            resampler = Resampler::new(source.rate, out_rate, out_channels);
        }
        mapped.clear();
        remap_channels(&chunk, source.channels, out_channels, &mut mapped);
        resampled.clear();
        resampler.process(&mapped, &mut resampled);

        let mut b = shared.buf.lock().unwrap_or_else(|e| e.into_inner());
        if shared.epoch.load(Ordering::Acquire) == epoch {
            b.queue.extend(resampled.iter().copied());
            if b.queue.len() >= prime {
                b.primed = true;
            }
        }
    }
}

impl Buf {
    fn epoch_matches(&self, epoch: &AtomicU64, seen: u64) -> bool {
        epoch.load(Ordering::Acquire) == seen
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<Shared>,
    channels: usize,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let shared = shared.clone();
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                let silence = T::from_sample(0.0f32);
                let Ok(mut b) = shared.buf.try_lock() else {
                    out.fill(silence);
                    return;
                };
                if shared.paused.load(Ordering::Relaxed) || !b.primed {
                    out.fill(silence);
                    return;
                }
                let mut taken = 0usize;
                for slot in out.iter_mut() {
                    match b.queue.pop_front() {
                        Some(v) => {
                            *slot = T::from_sample(v.clamp(-1.0, 1.0));
                            taken += 1;
                        }
                        None => *slot = silence,
                    }
                }
                b.played += (taken / channels.max(1)) as u64;
            },
            |_| {},
            None,
        )
        .map_err(|e| e.to_string())
}

// ---- The player the UI talks to ---------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Loading,
    Playing,
    Paused,
}

struct Track {
    peer: i64,
    id: i32,
    /// Seconds; from the message, so the bar is right before decoding starts.
    total: f64,
    phase: Phase,
    engine: Option<Engine>,
    path: Option<PathBuf>,
    /// Where to start once the download ends (a click on the bar).
    start: f64,
    percent: i32,
    /// Bumped per start, so a late download of an abandoned track is ignored.
    token: u64,
}

thread_local! {
    static TRACK: std::cell::RefCell<Option<Track>> = const { std::cell::RefCell::new(None) };
    static TIMER: slint::Timer = slint::Timer::default();
    static TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub fn wire(ui: &crate::AppWindow) {
    let player = ui.global::<Player>();
    player.on_toggle(toggle);
    player.on_seek(seek);
}

fn row_of(id: i32) -> Option<(i64, MessageRow)> {
    with_app(|app| {
        let peer = app.chat?;
        app.messages.iter().find(|m| m.id == id).cloned().map(|m| (peer, m))
    })
    .flatten()
}

fn with_track<R>(f: impl FnOnce(&mut Option<Track>) -> R) -> R {
    TRACK.with(|t| f(&mut t.borrow_mut()))
}

fn is_current(peer: i64, id: i32) -> bool {
    with_track(|t| t.as_ref().is_some_and(|t| t.peer == peer && t.id == id))
}

fn toggle(id: i32) {
    let Some((peer, row)) = row_of(id) else { return };
    if !is_track(&row) {
        return;
    }
    if !is_current(peer, id) {
        begin(peer, &row, 0.0);
        return;
    }
    with_track(|slot| {
        let Some(t) = slot else { return };
        match t.phase {
            // A second click while it downloads cancels it.
            Phase::Loading => *slot = None,
            Phase::Playing => {
                if let Some(e) = &t.engine {
                    e.set_paused(true);
                }
                t.phase = Phase::Paused;
            }
            Phase::Paused => {
                if let Some(e) = &t.engine {
                    e.set_paused(false);
                }
                t.phase = Phase::Playing;
            }
        }
    });
    publish();
}

/// A click or drag on the bar, as a fraction of its width.
fn seek(id: i32, fraction: f32) {
    let Some((peer, row)) = row_of(id) else { return };
    if !is_track(&row) {
        return;
    }
    let total = f64::from(row.duration);
    if !is_current(peer, id) {
        begin(peer, &row, seek_target(fraction, total));
        return;
    }
    let restart = with_track(|slot| {
        let t = slot.as_mut()?;
        let target = seek_target(fraction, t.total);
        t.start = target;
        match (&t.engine, t.phase) {
            (Some(e), _) if !e.finished() && !e.failed() => {
                e.seek(target);
                None
            }
            // Played to the end: a new engine at the clicked place.
            (Some(_), _) => t.path.clone().map(|p| (p, target)),
            // Still downloading: the start moved, nothing more.
            (None, _) => None,
        }
    });
    if let Some((path, target)) = restart {
        with_track(|slot| {
            if let Some(t) = slot {
                t.engine = Some(Engine::start(path, target, false));
                t.phase = Phase::Playing;
            }
        });
    }
    publish();
}

/// Stops whatever plays and starts `row`: download first, then decode.
fn begin(peer: i64, row: &MessageRow, start: f64) {
    let token = TOKEN.with(|t| {
        t.set(t.get() + 1);
        t.get()
    });
    let id = row.id;
    with_track(|slot| {
        // Dropping the old engine stops its thread.
        *slot = Some(Track {
            peer,
            id,
            total: f64::from(row.duration),
            phase: Phase::Loading,
            engine: None,
            path: None,
            start,
            percent: 0,
            token,
        });
    });
    ensure_timer();
    publish();

    let size = row.file_size.max(0) as u64;
    let last = Arc::new(AtomicI32::new(-1));
    run(
        async move {
            chats::message_file(tg(), peer, id, move |bytes| {
                if size == 0 {
                    return;
                }
                let percent = (bytes.min(size) * 100 / size) as i32;
                if last.swap(percent, Ordering::Relaxed) == percent {
                    return;
                }
                let _ = slint::invoke_from_event_loop(move || {
                    with_track(|slot| {
                        if let Some(t) = slot.as_mut().filter(|t| t.token == token) {
                            t.percent = percent;
                        }
                    });
                });
            })
            .await
        },
        move |result| {
            let ok = with_track(|slot| slot.as_ref().is_some_and(|t| t.token == token && t.phase == Phase::Loading));
            if !ok {
                return;
            }
            match result {
                Ok(path) => {
                    with_track(|slot| {
                        if let Some(t) = slot {
                            let path = PathBuf::from(path);
                            t.engine = Some(Engine::start(path.clone(), t.start, false));
                            t.path = Some(path);
                            t.phase = Phase::Playing;
                        }
                    });
                }
                Err(err) => {
                    crate::menus::toast(&format::error(&err));
                    with_track(|slot| *slot = None);
                }
            }
            publish();
        },
    );
}

fn ensure_timer() {
    TIMER.with(|timer| {
        if !timer.running() {
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), tick);
        }
    });
}

/// 10 times a second while a track exists: moves the bar, ends the track.
fn tick() {
    let problem = with_track(|slot| {
        let t = slot.as_ref()?;
        let engine = t.engine.as_ref()?;
        if engine.failed() {
            Some(true)
        } else if engine.finished() {
            Some(false)
        } else {
            None
        }
    });
    match problem {
        Some(true) => {
            crate::menus::toast(tr!("Could not play"));
            with_track(|slot| *slot = None);
        }
        // Played to the end: back to the idle row, as in Telegram.
        Some(false) => with_track(|slot| *slot = None),
        None => {}
    }
    publish();
    if with_track(|slot| slot.is_none()) {
        TIMER.with(|timer| timer.stop());
    }
}

/// Copies the player's state to the `Player` global the rows read.
fn publish() {
    let Some(ui) = ui() else { return };
    let player = ui.global::<Player>();
    let chat = with_app(|app| app.chat).flatten();
    with_track(|slot| match slot {
        Some(t) => {
            let position = t.engine.as_ref().map_or(t.start, |e| e.position());
            let shown = chat == Some(t.peer);
            player.set_shown(if shown { t.id } else { -1 });
            player.set_state(match t.phase {
                Phase::Loading => 1,
                Phase::Playing => 2,
                Phase::Paused => 3,
            });
            player.set_progress(fraction(position, t.total));
            player.set_elapsed(
                if t.phase == Phase::Loading { format!("{}%", t.percent) } else { fmt_time(position) }.into(),
            );
        }
        None => {
            player.set_shown(-1);
            player.set_state(0);
            player.set_progress(0.0);
            player.set_elapsed("".into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_five_bit_values() {
        // 0b00001 | 0b00010 << 5 | 0b00011 << 10 | 0b11111 << 15, little-endian.
        let packed: u32 = 1 | 2 << 5 | 3 << 10 | 31 << 15;
        let bytes = packed.to_le_bytes();
        let values = unpack_waveform(&bytes[..3]);
        assert_eq!(&values[..4], &[1, 2, 3, 31]);
        // 3 bytes hold 24 bits: four whole values.
        assert_eq!(values.len(), 4);
        assert!(unpack_waveform(&[]).is_empty());
    }

    #[test]
    fn bars_squeeze_and_normalise() {
        let values = [0u8, 31, 0, 31];
        let b = bars(&values, 2);
        assert_eq!(b.len(), 2);
        assert!((b[0] - 0.5).abs() < 1e-6 && (b[1] - 0.5).abs() < 1e-6);
        // Fewer values than bars: each value repeats.
        assert_eq!(bars(&[31], 4), vec![1.0; 4]);
        // A very quiet voice is not stretched to full height.
        assert!(bars(&[2, 2], 2)[0] < 0.5);
        assert!(bars(&[], 4).is_empty());
    }

    #[test]
    fn wave_path_has_one_segment_per_bar() {
        let path = wave_path(&[0.0, 1.0], 10.0, 20.0);
        assert_eq!(path.matches('M').count(), 2);
        assert!(path.starts_with("M 2.5 8.5 L 2.5 11.5"));
        assert!(wave_path(&[], 10.0, 10.0).is_empty());
    }

    #[test]
    fn formats_time() {
        assert_eq!(fmt_time(0.0), "0:00");
        assert_eq!(fmt_time(7.9), "0:07");
        assert_eq!(fmt_time(125.0), "2:05");
        assert_eq!(fmt_time(3723.0), "1:02:03");
        assert_eq!(fmt_time(-3.0), "0:00");
        assert_eq!(fmt_time(f64::NAN), "0:00");
    }

    #[test]
    fn seek_math() {
        assert_eq!(seek_target(0.5, 40.0), 20.0);
        assert_eq!(seek_target(-1.0, 40.0), 0.0);
        assert_eq!(seek_target(2.0, 40.0), 40.0);
        assert_eq!(seek_target(0.5, 0.0), 0.0);
        assert_eq!(fraction(10.0, 40.0), 0.25);
        assert_eq!(fraction(50.0, 40.0), 1.0);
        assert_eq!(fraction(5.0, 0.0), 0.0);
    }

    #[test]
    fn channel_mapping() {
        let mut out = Vec::new();
        remap_channels(&[1.0, 2.0], 1, 2, &mut out);
        assert_eq!(out, vec![1.0, 1.0, 2.0, 2.0]);
        out.clear();
        remap_channels(&[1.0, 3.0, 2.0, 4.0], 2, 1, &mut out);
        assert_eq!(out, vec![2.0, 3.0]);
        out.clear();
        remap_channels(&[1.0, 2.0, 3.0], 3, 2, &mut out);
        assert_eq!(out, vec![1.0, 2.0]);
    }

    #[test]
    fn resampler_keeps_duration() {
        let mut r = Resampler::new(48_000, 44_100, 1);
        let input: Vec<f32> = (0..4800).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // 100 ms stays 100 ms (give or take the one frame held back).
        assert!((out.len() as i64 - 4410).abs() <= 2, "{}", out.len());
        // A ramp stays a ramp.
        assert!(out.windows(2).all(|w| w[1] > w[0]));
        let mut same = Resampler::new(48_000, 48_000, 2);
        let mut o = Vec::new();
        same.process(&[1.0, 2.0, 3.0, 4.0], &mut o);
        assert_eq!(o, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn plays_only_audio_with_a_file() {
        let mut m = MessageRow {
            id: 1,
            peer_id: 1,
            sender_id: None,
            sender_name: String::new(),
            date: 0,
            text: String::new(),
            kind: "voice".into(),
            outgoing: false,
            edited: false,
            image_w: 0,
            image_h: 0,
            file_name: String::new(),
            file_size: 0,
            playable: true,
            duration: 65,
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
        };
        let v = item(&m);
        assert_eq!((v.kind, v.total.as_str()), (1, "1:05"));
        m.kind = "audio".into();
        m.file_name = "song.mp3".into();
        assert_eq!(item(&m).title, "song.mp3");
        m.track = "Artist — Song".into();
        assert_eq!(item(&m).title, "Artist — Song");
        m.playable = false;
        assert_eq!(item(&m).kind, 0);
    }

    /// `OXIGRAM_AUDIO_SAMPLES=<dir>` with 3 s files (ogg/opus, mp3, m4a, ...):
    /// each must decode to about 3 s and survive a seek.
    #[test]
    #[ignore]
    fn decodes_sample_files() {
        let dir = std::env::var("OXIGRAM_AUDIO_SAMPLES").expect("set OXIGRAM_AUDIO_SAMPLES");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let mut src = Source::open(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            let mut out = Vec::new();
            while src.read(&mut out).unwrap() {}
            let secs = out.len() as f64 / (src.rate as f64 * src.channels as f64);
            println!("{path:?}: {secs:.2}s at {} Hz x{}", src.rate, src.channels);
            assert!((2.7..3.4).contains(&secs), "{path:?}: {secs}");
            assert!(out.iter().any(|v| v.abs() > 0.01), "{path:?} is silent");
            // A 440 Hz sine crosses zero ~880 times a second (first channel).
            let ch = src.channels;
            let first: Vec<f32> = out.iter().step_by(ch).copied().collect();
            let crossings = first.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
            let per_second = crossings as f64 / secs;
            println!("  zero crossings/s: {per_second:.0}");
            assert!((780.0..980.0).contains(&per_second), "{path:?}: {per_second}");
            src.seek(1.5);
            let mut tail = Vec::new();
            while src.read(&mut tail).unwrap() {}
            let rest = tail.len() as f64 / (src.rate as f64 * src.channels as f64);
            assert!((1.2..1.8).contains(&rest), "{path:?} after seek: {rest}");
        }
    }
}
