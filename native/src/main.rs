//! Oxigram with a native Slint window instead of a WebView.
//!
//! Threads: the Slint event loop owns the window and all view state (`App`,
//! kept in a thread-local); a small tokio runtime runs `oxigram-core`. Work
//! goes to tokio with [`run`], and results come back through
//! `slint::invoke_from_event_loop`, so the view state is only ever touched on
//! the UI thread and needs no locks.

// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod autostart;
mod buttons;
mod contacts;
mod context_menu;
mod drafts;
mod files;
mod folders;
mod commands;
mod countries;
mod format;
mod i18n;
mod links;
mod menus;
mod messages;
mod pictures;
mod replies;
mod notify;
mod passcode;
mod phone;
mod profile;
mod proxy;
mod reactions;
mod search;
mod select;
mod sending;
mod settings;
mod signin;
mod single;
mod tray;
mod typing;
mod update;
#[cfg(test)]
mod layout_shot;
mod video;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use oxigram_core::chats;
use oxigram_core::error::UiError;
use oxigram_core::store::chats::{DialogRow, MessageRow};
use oxigram_core::tg::{Telegram, UiState};
use oxigram_core::{tr, trn};
use oxigram_core::updates::{EventSink, UiEvent};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

/// Same folders as the Tauri build (its bundle identifier), so both share the
/// keys, the session and the cache. The lock in `oxigram-core` keeps them
/// from running at the same time.
const APP_ID: &str = "app.oxigram.desktop";
const HISTORY_PAGE: usize = 50;
const TITLE_EVERY: Duration = Duration::from_secs(3);
/// Messages closer than this (seconds) from one sender form one run.
const RUN_GAP: i64 = 300;
/// Decoded avatars kept in memory; the files stay in the disk cache.
const AVATAR_CACHE: usize = 160;
/// The largest avatar drawn (the list's 48 px circle), in logical pixels.
const AVATAR_PX: f32 = 48.0;
/// Background paging stops here; scrolling further still loads more.
const MAX_DIALOGS: usize = 2000;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static TELEGRAM: OnceLock<Telegram> = OnceLock::new();

fn tg() -> &'static Telegram {
    TELEGRAM.get().expect("set in main")
}

/// Runs `work` on tokio and hands its result to `done` on the UI thread.
fn run<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    RUNTIME.get().expect("set in main").spawn(async move {
        let value = work.await;
        let _ = slint::invoke_from_event_loop(move || done(value));
    });
}

/// View state, owned by the UI thread.
struct App {
    ui: slint::Weak<AppWindow>,
    /// Who is typing where, pruned by a 1 s timer.
    typing: typing::Typing,
    /// Members and online of the open group, kept for the header when typing ends.
    members: std::cell::Cell<Option<(i64, (Option<i32>, Option<i32>))>>,
    /// Every dialog, in list order (pinned first, then newest).
    dialogs: Vec<DialogRow>,
    /// Peer IDs of the rows on screen, which search and the archive narrow.
    shown: Vec<i64>,
    dialog_model: Rc<VecModel<DialogItem>>,
    filter: String,
    show_archive: bool,
    dialogs_done: bool,
    dialogs_loading: bool,
    /// Avatars shrunk to their circle, most recently used last.
    avatars: HashMap<i64, slint::Image>,
    avatar_order: VecDeque<i64>,
    avatar_pending: HashSet<i64>,
    chat: Option<i64>,
    /// The open forum topic, when the chat is a forum.
    topic: Option<i32>,
    topic_title: String,
    topics: Vec<oxigram_core::chats::TopicRow>,
    topics_open: bool,
    forum: bool,
    /// Slash commands of the open bot chat.
    bot_commands: Vec<(String, String)>,
    /// The open chat's messages, oldest first, one per row of `message_model`.
    messages: Vec<MessageRow>,
    message_model: Rc<VecModel<MessageItem>>,
    history_done: bool,
    history_loading: bool,
    /// Bumped when another chat opens, so late replies for the old one are dropped.
    generation: u64,
    compose: messages::Compose,
    /// Peer IDs of the forward picker's rows, and the messages it forwards.
    forward_shown: Vec<i64>,
    forward_ids: Vec<i32>,
    /// Waits for the confirm dialog; gets the checkbox state when accepted.
    pending: Option<Box<dyn FnOnce(bool)>>,
    me_id: i64,
    me_photo_id: Option<String>,
    settings: settings::Settings,
    /// The top chats are prefetched once per start.
    prefetched: bool,
    /// What the open right-click menu acts on.
    menu_target: Option<context_menu::MenuTarget>,
    /// Decoded pictures of the open chat's messages.
    thumbs: pictures::Thumbs,
    /// The full-window picture viewer, when open.
    viewer: Option<pictures::Viewer>,
    /// Pictures of custom emoji reactions.
    emoji: reactions::Emoji,
    /// Which reactions the open chat allows.
    allowed: reactions::Allowed,
    /// Width of the widest bubble, for wrapping reaction chips.
    reaction_width: f32,
    /// Bubbles picked in selection mode, by their first message's ID.
    selected: Vec<i32>,
    /// Search results and the jump window.
    search: search::State,
    /// Bumped when the account is forgotten (lock, sign-out), so dialog
    /// pages still on their way for the old one are dropped.
    dialogs_epoch: u64,
    /// Quotes of replies to messages that are not loaded.
    replies: replies::Replies,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the view state. Slint may call back into Rust while a model
/// is being changed; such nested calls are skipped rather than panicking.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|app| match app.try_borrow_mut() {
        Ok(mut app) => app.as_mut().map(f),
        Err(_) => None,
    })
}

/// Live events from the update task, delivered on the UI thread.
struct WindowSink;

impl EventSink for WindowSink {
    fn send(&self, events: Vec<UiEvent>) -> bool {
        slint::invoke_from_event_loop(move || {
            with_app(|app| app.apply_events(events));
        })
        .is_ok()
    }
}

/// Picks the renderer once, before the first window: OpenGL (femtovg) when the
/// "GPU rendering" setting is on, software otherwise. `SLINT_BACKEND` in the
/// environment overrides this (used for measurements).
fn select_renderer() {
    if std::env::var_os("SLINT_BACKEND").is_some() {
        return;
    }
    let select = |renderer: &str| {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name(renderer.into())
            .select()
    };
    #[cfg(feature = "gpu")]
    if settings::Settings::load().gpu_render && select("femtovg").is_ok() {
        return;
    }
    let _ = select("software");
}

fn main() {
    // OXIGRAM_HOME puts everything in one folder: a clean profile for tests
    // and measurements that never touches the real account.
    let dirs = match std::env::var_os("OXIGRAM_HOME") {
        Some(home) => {
            let home = std::path::PathBuf::from(home);
            (Some(home.clone()), Some(home.clone()), Some(home))
        }
        None => (dirs::config_dir(), dirs::data_dir(), dirs::cache_dir()),
    };
    let (Some(config), Some(data), Some(cache)) = dirs else {
        eprintln!("no per-user folders on this system");
        return;
    };
    // One copy per profile: a second launch wakes the first and leaves.
    if matches!(single::claim(&config.join(APP_ID)), single::Role::Second) {
        return;
    }
    settings::set_dir(config.join(APP_ID));
    drafts::load();
    let _ = TELEGRAM.set(Telegram::new(
        config.join(APP_ID),
        data.join(APP_ID),
        cache.join(APP_ID).join("media"),
    ));
    // Two workers are plenty for one connection and a few downloads.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _ = RUNTIME.set(runtime);

    select_renderer();
    let ui = AppWindow::new().expect("window");
    // The language: the saved choice, or the system's.
    i18n::install();
    i18n::set_language(&settings::Settings::load().language);
    let dialog_model = Rc::new(VecModel::default());
    let message_model = Rc::new(VecModel::default());
    ui.set_dialogs(ModelRc::from(dialog_model.clone()));
    ui.set_messages(ModelRc::from(message_model.clone()));
    APP.with(|app| {
        *app.borrow_mut() = Some(App {
            typing: typing::Typing::default(),
            members: std::cell::Cell::new(None),
            ui: ui.as_weak(),
            dialogs: Vec::new(),
            shown: Vec::new(),
            dialog_model,
            filter: String::new(),
            show_archive: false,
            dialogs_done: false,
            dialogs_loading: false,
            avatars: HashMap::new(),
            avatar_order: VecDeque::new(),
            avatar_pending: HashSet::new(),
            chat: None,
            topic: None,
            topic_title: String::new(),
            topics: Vec::new(),
            topics_open: false,
            forum: false,
            bot_commands: Vec::new(),
            messages: Vec::new(),
            message_model,
            history_done: false,
            history_loading: false,
            generation: 0,
            compose: messages::Compose::None,
            forward_shown: Vec::new(),
            forward_ids: Vec::new(),
            pending: None,
            me_id: 0,
            me_photo_id: None,
            settings: settings::Settings::load(),
            prefetched: false,
            menu_target: None,
            thumbs: pictures::Thumbs::default(),
            viewer: None,
            emoji: reactions::Emoji::default(),
            allowed: reactions::Allowed::default(),
            reaction_width: 300.0,
            selected: Vec::new(),
            search: search::State::new(&ui),
            replies: replies::Replies::default(),
            dialogs_epoch: 0,
        })
    });
    let bench = std::env::var_os("OXIGRAM_BENCH").is_some();
    ui.set_bench(bench);
    // In the benchmark, a 1 ms timer measures the event loop's longest
    // stalls: a slow frame (or row creation) delays the next tick. Average
    // FPS hides such hitches.
    let stall_timer = slint::Timer::default();
    if bench {
        let mut last = std::time::Instant::now();
        let mut window_start = last;
        let mut worst = Duration::ZERO;
        let mut hitches = 0u32;
        stall_timer.start(slint::TimerMode::Repeated, Duration::from_millis(1), move || {
            let now = std::time::Instant::now();
            let gap = now - last;
            last = now;
            worst = worst.max(gap);
            if gap > Duration::from_millis(25) {
                hitches += 1;
            }
            if now - window_start >= Duration::from_secs(1) {
                eprintln!("stall: worst {} ms, hitches>25ms {hitches}", worst.as_millis());
                window_start = now;
                worst = Duration::ZERO;
                hitches = 0;
            }
        });
    }
    wire_login(&ui);
    wire_main(&ui);
    menus::wire(&ui);
    settings::wire(&ui);
    update::wire(&ui);
    passcode::wire(&ui);
    phone::wire(&ui);
    signin::wire(&ui);
    messages::wire(&ui);
    search::wire(&ui);
    sending::wire(&ui);
    context_menu::wire(&ui);
    pictures::wire(&ui);
    files::wire(&ui);
    links::wire(&ui);
    buttons::wire(&ui);
    ui.on_open_discussion(open_discussion);
    let weak = ui.as_weak();
    ui.on_latin_shortcut(move |text| {
        let Some(latin) = latin_shortcut(&text) else { return false };
        let weak = weak.clone();
        // After this key event is done, so the field gets it as a fresh one
        // (Ctrl is still held, so it arrives as Ctrl+letter).
        slint::Timer::single_shot(Duration::ZERO, move || {
            let Some(ui) = weak.upgrade() else { return };
            let text = SharedString::from(latin);
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
            ui.window().dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
        });
        true
    });
    // Ctrl+Tab: the next (or previous) chat of the list on screen.
    ui.on_next_chat(|forward| {
        let next = with_app(|app| {
            if app.shown.is_empty() {
                return None;
            }
            let len = app.shown.len();
            let at = app.chat.and_then(|peer| app.shown.iter().position(|p| *p == peer));
            let index = match at {
                None => 0,
                Some(i) if forward => (i + 1) % len,
                Some(i) => (i + len - 1) % len,
            };
            app.shown.get(index).copied()
        })
        .flatten();
        if let Some(peer) = next {
            open_chat(peer);
        }
    });
    // Up in an empty field: edit the last own text message.
    ui.on_edit_last(|| {
        let last = with_app(|app| {
            app.messages
                .iter()
                .rev()
                .find(|m| m.outgoing && m.kind == "text" && m.grouped_id.is_none())
                .cloned()
        })
        .flatten();
        if let Some(row) = last {
            messages::start_edit(&row);
        }
    });
    audio::wire(&ui);
    reactions::wire(&ui);
    select::wire(&ui);
    proxy::wire(&ui);
    folders::wire(&ui);
    drafts::wire(&ui);
    contacts::wire(&ui);
    profile::wire(&ui);

    // Expires "печатает…" entries; does nothing while nobody types.
    let typing_timer = slint::Timer::default();
    typing_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), || {
        with_app(|app| app.prune_typing());
    });

    // "Oxigram — 31 МБ ОЗУ", measured like the Tauri build does.
    let title_timer = slint::Timer::default();
    let weak = ui.as_weak();
    title_timer.start(slint::TimerMode::Repeated, TITLE_EVERY, move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Some(info) = oxigram_core::mem::usage() {
            let unread: i32 = with_app(|app| app.unread_total()).unwrap_or(0);
            let count = if unread > 0 { format!(" ({unread})") } else { String::new() };
            ui.set_window_title(
                format!("Oxigram{count} — {}", tr!("{} MB of RAM", info.bytes / (1024 * 1024))).into(),
            );
        }
    });

    tray::install_notifier(&tg().ui);
    login_step(async { tg().state().await });
    // Shown before the tray hooks the window's events; the loop outlives the
    // window so the app can sit in the tray.
    ui.show().expect("window");
    let _tray_timer = tray::init(&ui);
    single::listen(&ui);
    autostart::sync(with_app(|app| app.settings.autostart).unwrap_or(false));
    if autostart::launched_by_system() {
        // Quietly in the tray; without a tray, minimized.
        if tray::available() {
            let _ = ui.hide();
        } else {
            ui.window().set_minimized(true);
        }
    }
    slint::run_event_loop_until_quit().expect("event loop");
}

/// After the language changed: rebuilds what Rust wrote into the window.
pub(crate) fn relabel() {
    with_app(|app| {
        app.refresh_dialogs();
        let items: Vec<MessageItem> = (0..app.messages.len()).map(|i| app.item(i)).collect();
        app.message_model.set_vec(items);
        if let Some(ui) = app.ui.upgrade() {
            app.show_chat_rights(&ui);
        }
    });
    if let Some(ui) = ui() {
        settings::fill(&ui);
        proxy::show_summary(&ui);
        passcode::refresh(&ui);
        phone::relabel(&ui);
    }
}

/// Forgets what the window shows of the account (on lock): chats, messages,
/// pictures, drafts, open panels.
fn forget_view() {
    pictures::close_viewer();
    drafts::forget_all();
    with_app(|app| {
        app.chat = None;
        app.generation += 1;
        app.dialogs.clear();
        app.shown.clear();
        app.dialog_model.set_vec(Vec::new());
        app.messages.clear();
        app.message_model.set_vec(Vec::new());
        app.thumbs.clear();
        app.avatars.clear();
        app.avatar_order.clear();
        app.avatar_pending.clear();
        app.replies.clear();
        app.selected.clear();
        app.compose = messages::Compose::None;
        app.dialogs_done = false;
        app.dialogs_loading = false;
        app.dialogs_epoch += 1;
        app.prefetched = false;
        if let Some(ui) = app.ui.upgrade() {
            app.search.reset(&ui);
            ui.set_chat_open(false);
            ui.set_settings_open(false);
            ui.set_drawer_open(false);
            ui.set_info_open(false);
            ui.invoke_set_composer(SharedString::new());
        }
    });
}

fn ui() -> Option<AppWindow> {
    with_app(|app| app.ui.upgrade()).flatten()
}

// ---- Login ---------------------------------------------------------------

fn wire_login(ui: &AppWindow) {
    ui.on_save_keys(|id, hash| {
        let id: i32 = id.trim().parse().unwrap_or(0);
        let hash = hash.to_string();
        login_step(async move { tg().save_keys(id, &hash).await });
    });
    ui.on_request_code(|phone| {
        // "+<country code><number>" from the country picker and the field.
        let phone = phone::full_number(&phone);
        login_step(async move { tg().request_code(&phone).await });
    });
    ui.on_submit_code(|code| {
        let code = code.to_string();
        login_step(async move { tg().submit_code(&code).await });
    });
    ui.on_submit_password(|password| {
        let password = password.to_string();
        login_step(async move { tg().submit_password(&password).await });
    });
    ui.on_back_to_phone(|| login_step(async { tg().back_to_phone().await }));
    ui.on_change_keys(|| login_step(async { tg().clear_keys().await }));
}

pub(crate) fn login_step(step: impl Future<Output = Result<UiState, UiError>> + Send + 'static) {
    if let Some(ui) = ui() {
        ui.set_busy(true);
        ui.set_error(SharedString::new());
        ui.set_keys_rejected(false);
    }
    run(step, show_state);
}

/// A QR poll: a new code or the next step; errors only when it is not a
/// passing network hiccup.
pub(crate) fn show_state_quiet(result: Result<UiState, UiError>) {
    match result {
        Ok(state) => show_state(Ok(state)),
        // A passing network hiccup or a flood wait: the next poll retries.
        Err(err) if err.code == "NETWORK" || err.wait_seconds.is_some() => {}
        Err(err) => show_state(Err(err)),
    }
}

fn show_state(result: Result<UiState, UiError>) {
    let Some(ui) = ui() else { return };
    ui.set_busy(false);
    let state = match result {
        Ok(state) => state,
        Err(err) => {
            ui.set_error(format::error(&err).into());
            let code = err.code.as_str();
            ui.set_keys_rejected(code == "API_ID_INVALID" || code == "API_ID_PUBLISHED_FLOOD");
            // The backend dropped the pending step: resync with it.
            if code == "WRONG_STAGE" || code == "PHONE_CODE_EXPIRED" {
                run(async { tg().state().await }, |state| {
                    if let Ok(state) = state {
                        show_state(Ok(state));
                    }
                });
            }
            if ui.get_stage() == "loading" {
                // No screen to keep yet (offline at start): retry shortly.
                slint::Timer::single_shot(Duration::from_secs(5), || {
                    login_step(async { tg().state().await });
                });
            }
            return;
        }
    };
    let stage = match state {
        UiState::Locked => {
            passcode::refresh(&ui);
            "locked"
        }
        UiState::NeedKeys => "keys",
        UiState::NeedPhone => "phone",
        UiState::NeedCode { sent } => {
            signin::show_code(&ui, &sent);
            "code"
        }
        UiState::NeedQr { path, size } => {
            signin::show_qr(&ui, &path, size);
            "qr"
        }
        UiState::NeedPassword { hint } => {
            ui.set_login_hint(hint.unwrap_or_default().into());
            "password"
        }
        UiState::Ready { me } => {
            menus::show_me(&me);
            update::at_start();
            if ui.get_stage() != "ready" {
                tg().ui.attach(Box::new(WindowSink));
                load_dialogs();
                folders::load();
            }
            "ready"
        }
    };
    if stage != "qr" {
        signin::left_qr();
    }
    ui.set_stage(stage.into());
}

// ---- Dialog list ---------------------------------------------------------

fn wire_main(ui: &AppWindow) {
    // Debug: open the first bot chat and type "/" so the command list can be
    // checked without a pointer.
    if std::env::var("OXIGRAM_SHOW_COMMANDS").is_ok() {
        slint::Timer::single_shot(std::time::Duration::from_secs(5), || {
            let target = with_app(|app| app.dialogs.iter().find(|d| d.bot).map(|d| d.id)).flatten();
            if let Some(peer) = target {
                open_chat_at(peer, None);
                slint::Timer::single_shot(std::time::Duration::from_secs(3), || {
                    with_app(|app| {
                        if let Some(ui) = app.ui.upgrade() {
                            ui.invoke_set_composer("/".into());
                        }
                    });
                });
            }
        });
    }
    ui.on_open_topics(open_topics);
    ui.on_pick_topic(pick_topic);
    ui.on_close_topic(close_topic);
    ui.on_pick_command(commands::pick);
    ui.on_open_chat(|index| {
        let peer = with_app(|app| app.shown.get(index as usize).copied()).flatten();
        if let Some(peer) = peer {
            open_chat(peer);
        }
    });
    ui.on_filter_dialogs(|text| {
        with_app(|app| {
            app.filter = text.to_lowercase();
            app.refresh_dialogs();
        });
    });
    ui.on_toggle_archive(|| {
        with_app(|app| {
            app.show_archive = !app.show_archive;
            app.refresh_dialogs();
        });
    });
    ui.on_want_avatar(|index| {
        let peer = with_app(|app| app.shown.get(index as usize).copied()).flatten();
        if let Some(peer) = peer {
            request_avatar(peer);
        }
    });
    ui.on_load_more_dialogs(more_dialogs);
    ui.on_load_older(load_older);
    // Esc with nothing else open: hide the chat, as Telegram Desktop does.
    ui.on_close_chat(|| {
        pictures::close_viewer();
        with_app(|app| {
            app.chat = None;
            app.generation += 1;
            app.compose = messages::Compose::None;
            app.thumbs.clear();
            app.selected.clear();
            if let Some(ui) = app.ui.upgrade() {
                ui.set_select_mode(false);
            }
            app.set_messages(Vec::new());
            if let Some(ui) = app.ui.upgrade() {
                app.search.reset(&ui);
                ui.set_chat_open(false);
                ui.set_info_open(false);
                ui.set_bar_mode(SharedString::new());
                ui.set_selecting_id(-1);
            }
            app.refresh_dialogs();
        });
        tg().ui.set_active_chat(None);
    });
    ui.on_toggle_mute(|| {
        if let Some(Some(peer)) = with_app(|app| app.chat) {
            menus::dialog_action(peer, "mute");
        }
    });
}

/// Shows the cached list at once, then the first page from Telegram.
fn load_dialogs() {
    let epoch = with_app(|app| app.dialogs_epoch).unwrap_or(0);
    run(async { chats::cached_dialogs(tg()).await }, move |cached| {
        with_app(|app| {
            if app.dialogs_epoch != epoch {
                return;
            }
            if let Ok(rows) = cached {
                app.dialogs = rows;
                app.sort_dialogs();
                app.refresh_dialogs();
            }
            app.dialogs_loading = true;
        });
        run(async { chats::load_dialogs(tg(), true).await }, move |page| {
            with_app(|app| {
                if app.dialogs_epoch != epoch {
                    return;
                }
                app.dialogs_loading = false;
                match page {
                    Ok(page) => {
                        app.dialogs_done = page.done;
                        // Fresh rows replace their cached copies; cached chats
                        // further down stay until their page arrives.
                        let fresh: HashSet<i64> = page.dialogs.iter().map(|d| d.id).collect();
                        app.dialogs.retain(|d| !fresh.contains(&d.id));
                        let mut rows = page.dialogs;
                        rows.append(&mut app.dialogs);
                        app.dialogs = rows;
                        app.sort_dialogs();
                        app.refresh_dialogs();
                    }
                    Err(err) => app.chat_error(&err),
                }
            });
            prefetch_top();
            more_dialogs();
        });
    });
}

/// Fills the SQLite cache with the first page of the top chats, one after
/// another, so they open without waiting for the network. Read-only: it never
/// marks anything as read.
pub(crate) fn prefetch_top() {
    let peers = with_app(|app| {
        if !app.settings.prefetch_chats || app.prefetched || app.dialogs.is_empty() {
            return Vec::new();
        }
        app.prefetched = true;
        app.dialogs
            .iter()
            .filter(|d| !d.archived)
            .take(settings::PREFETCH_CHATS)
            .map(|d| d.id)
            .collect()
    })
    .unwrap_or_default();
    if peers.is_empty() {
        return;
    }
    RUNTIME.get().expect("set in main").spawn(async move {
        for peer in peers {
            let _ = chats::history(tg(), peer, None, settings::PREFETCH_PAGE).await;
        }
    });
}

fn more_dialogs() {
    let go = with_app(|app| {
        let go = !app.dialogs_loading && !app.dialogs_done;
        app.dialogs_loading |= go;
        go
    });
    if go != Some(true) {
        return;
    }
    let epoch = with_app(|app| app.dialogs_epoch).unwrap_or(0);
    run(async { chats::load_dialogs(tg(), false).await }, move |page| {
        with_app(|app| {
            // A page for the account before a sign-out: dropped.
            if app.dialogs_epoch != epoch {
                return;
            }
            app.dialogs_loading = false;
            let Ok(page) = page else { return };
            app.dialogs_done = page.done;
            for row in page.dialogs {
                match app.dialogs.iter_mut().find(|d| d.id == row.id) {
                    Some(existing) => *existing = row,
                    None => app.dialogs.push(row),
                }
            }
            app.sort_dialogs();
            app.refresh_dialogs();
        });
        // The whole list (archive included) is small: keep paging in the
        // background so counts and search see every chat, not only the first page.
        let more = with_app(|app| !app.dialogs_done && app.dialogs.len() < MAX_DIALOGS);
        if more == Some(true) {
            slint::Timer::single_shot(Duration::from_millis(300), more_dialogs);
        }
    });
}

/// Fetches a chat photo (from the disk cache when it is there) and shrinks it
/// to the size it is drawn at.
fn request_avatar(peer: i64) {
    let Some(Some((photo_id, px))) = with_app(|app| {
        // "Фото на аватарках" off: nothing is fetched at all.
        if !app.settings.avatars || app.avatars.contains_key(&peer) || app.avatar_pending.contains(&peer) {
            return None;
        }
        let dialog = app.dialog(peer)?;
        if dialog.kind == "saved" {
            return None;
        }
        let photo_id = dialog.photo_id.clone()?;
        app.avatar_pending.insert(peer);
        let scale = app.ui.upgrade().map_or(1.0, |ui| ui.window().scale_factor());
        Some((photo_id, (AVATAR_PX * scale).ceil() as u32))
    }) else {
        return;
    };
    run(
        async move {
            let path = chats::avatar(tg(), peer, &photo_id).await.ok()?;
            tokio::task::spawn_blocking(move || {
                let picture = crate::pictures::open_image(path).ok()?;
                let small = picture.resize_to_fill(px, px, image::imageops::FilterType::Triangle);
                Some(circle(small.to_rgba8()))
            })
            .await
            .ok()
            .flatten()
        },
        move |pixels| {
            with_app(|app| {
                // A failure stays "pending", so the photo is not retried in a loop.
                let Some(pixels) = pixels else { return };
                app.avatar_pending.remove(&peer);
                // Switched off while it was downloading.
                if !app.settings.avatars {
                    return;
                }
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    pixels.as_raw(),
                    pixels.width(),
                    pixels.height(),
                );
                app.remember_avatar(peer, slint::Image::from_rgba8(buffer));
            });
        },
    );
}

/// Cuts the square photo to a circle with a soft edge. The software renderer
/// does not clip images to rounded corners, so the mask is in the pixels.
fn circle(mut pixels: image::RgbaImage) -> image::RgbaImage {
    let size = pixels.width() as f32;
    let radius = size / 2.0;
    for (x, y, pixel) in pixels.enumerate_pixels_mut() {
        let dx = x as f32 + 0.5 - radius;
        let dy = y as f32 + 0.5 - radius;
        let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
        pixel.0[3] = (pixel.0[3] as f32 * coverage).round() as u8;
    }
    pixels
}

// ---- Open chat -----------------------------------------------------------

fn open_chat(peer: i64) {
    open_chat_at(peer, None);
}

/// Opens a chat; with `target`, around that message instead of at the end.
/// "12 comments" under a channel post: the discussion group, opened at the
/// message that holds the thread.
fn open_discussion(message_id: i32) {
    let Some(Some(peer)) = with_app(|app| app.chat) else { return };
    run(
        async move { oxigram_core::chats::discussion(tg(), peer, message_id).await },
        |result| match result {
            Ok((peer_id, id)) => open_chat_at(peer_id, Some(id)),
            Err(err) => menus::toast(format::error(&err)),
        },
    );
}

/// The comments label of a channel post, empty when it has no discussion.
fn comments_text(comments: i32) -> String {
    if comments <= 0 {
        return String::new();
    }
    oxigram_core::trn!("{n} comment", "{n} comments", comments)
}

fn open_chat_at(peer: i64, target: Option<i32>) {    pictures::close_viewer();
    let Some(generation) = with_app(|app| {
        app.chat = Some(peer);
        app.generation += 1;
        app.topic = None;
        app.topic_title.clear();
        app.topics.clear();
        app.topics_open = false;
        app.forum = false;
        app.messages.clear();
        app.message_model.clear();
        app.thumbs.clear();
        app.replies.clear();
        app.selected.clear();
        if let Some(ui) = app.ui.upgrade() {
            ui.set_select_mode(false);
        }
        app.history_done = false;
        app.history_loading = true;
        app.compose = messages::Compose::None;
        if let Some(ui) = app.ui.upgrade() {
            app.search.reset(&ui);
            ui.set_bar_mode(SharedString::new());
            ui.set_selecting_id(-1);
            let (title, saved) = app
                .dialog(peer)
                .map(|d| (format::dialog_title(d), d.kind == "saved"))
                .unwrap_or_default();
            ui.set_chat_initial(format::initial(&title).into());
            ui.set_chat_title(title.into());
            ui.set_chat_saved(saved);
            ui.set_chat_color(format::avatar_color(peer));
            app.show_chat_photo(&ui);
            app.show_subtitle(&ui, None);
            ui.set_chat_error(SharedString::new());
            ui.set_chat_forum(false);
            ui.set_topic_open(false);
            ui.set_topic_title(SharedString::new());
            ui.set_topic_items(ModelRc::from(std::rc::Rc::new(VecModel::<TopicItem>::from(Vec::new()))));
            app.bot_commands.clear();
            ui.set_commands_open(false);
            ui.set_command_items(ModelRc::from(std::rc::Rc::new(VecModel::<CommandItem>::from(
                Vec::new(),
            ))));
            app.show_chat_rights(&ui);
            ui.set_chat_open(true);
            ui.set_history_loading(true);
            // Its draft, if the chat has one.
            ui.invoke_set_composer(drafts::get(peer).into());
            ui.invoke_scroll_to_bottom();
            // Type right away, without clicking the field (not in read-only channels).
            if ui.get_chat_can_post() {
                ui.invoke_focus_composer();
            } else {
                // No field to focus: the window's key handler takes focus,
                // or Esc (close the chat) would reach nothing.
                ui.invoke_focus_keys();
            }
        }
        app.allowed = reactions::Allowed::Unknown;
        app.refresh_dialogs();
        app.generation
    }) else {
        return;
    };
    reactions::load_allowed(peer, generation);
    profile::chat_changed();
    tg().ui.set_active_chat(Some(peer));
    request_avatar(peer);

    // Member counts for groups and channels come with the full profile, and a
    // bot's slash commands do too.
    let (kind, bot) = with_app(|app| {
        let dialog = app.dialog(peer);
        (dialog.map(|d| d.kind.clone()), dialog.is_some_and(|d| d.bot))
    })
    .unwrap_or_default();
    if bot || kind.as_deref() != Some("user") {
        run(async move { chats::peer_info(tg(), peer).await }, move |info| {
            with_app(|app| {
                if app.generation != generation {
                    return;
                }
                if let (Ok(info), Some(ui)) = (info, app.ui.upgrade()) {
                    app.show_subtitle(&ui, Some((info.members, info.online)));
                    app.bot_commands = info.commands.clone();
                    let text = ui.get_composer_text();
                    commands::suggest(&ui, &app.bot_commands, text.as_str());
                    if info.forum {
                        app.forum = true;
                        app.topics_open = true;
                        ui.set_chat_forum(true);
                        ui.set_topics_open(true);
                    }
                }
            });
            if with_app(|app| app.forum && app.generation == generation).unwrap_or(false) {
                load_topics(peer, generation);
                mark_read(peer);
            }
        });
    }

    // Jumping to a found message shows the history around it; the chat is
    // not marked read, as nothing at its end has been seen.
    if let Some(target) = target {
        search::load_window(peer, target, generation);
        return;
    }
    let unread = with_app(|app| app.dialog(peer).is_some_and(|d| d.unread > 0)).unwrap_or(false);
    run(
        async move { chats::cached_history(tg(), peer, None, HISTORY_PAGE).await },
        move |cached| {
            with_app(|app| {
                if app.generation != generation {
                    return;
                }
                if let Ok(mut rows) = cached {
                    rows.reverse();
                    app.set_messages(rows);
                }
            });
            run(
                async move { chats::history(tg(), peer, None, HISTORY_PAGE).await },
                move |fresh| {
                    with_app(|app| {
                        if app.generation != generation {
                            return;
                        }
                        app.history_loading = false;
                        match fresh {
                            Ok(mut rows) => {
                                app.history_done = rows.len() < HISTORY_PAGE;
                                rows.reverse();
                                app.set_messages(rows);
                            }
                            Err(err) => app.chat_error(&err),
                        }
                        if let Some(ui) = app.ui.upgrade() {
                            ui.set_history_loading(false);
                        }
                    });
                },
            );
        },
    );
    // Opening a chat reads it, as in the official apps.
    if unread {
        mark_read(peer);
    }
}

fn load_topics(peer: i64, generation: u64) {
    run(async move { chats::topics(tg(), peer).await }, move |found| {
        let missing = with_app(|app| {
            if app.generation != generation {
                return Vec::new();
            }
            let rows = match found {
                Ok(rows) => rows,
                Err(err) => {
                    app.chat_error(&err);
                    return Vec::new();
                }
            };
            app.topics = rows;
            apply_topics(app);
            missing_icons(app)
        });
        for id in missing.unwrap_or_default() {
            reactions::request_emoji(id);
        }
    });
}

/// The topic rows with the icons they already have decoded.
fn topic_items(app: &App) -> Vec<TopicItem> {
    app.topics
        .iter()
        .map(|topic| TopicItem {
            id: topic.id,
            title: topic.title.clone().into(),
            emoji: topic
                .icon_emoji_id
                .as_ref()
                .and_then(|id| app.emoji.images.get(id))
                .cloned()
                .unwrap_or_default(),
            unread: topic.unread,
            pinned: topic.pinned,
            closed: topic.closed,
            color: format::avatar_color(i64::from(topic.icon_color)),
        })
        .collect()
}

fn apply_topics(app: &mut App) {
    let items = topic_items(app);
    if let Some(ui) = app.ui.upgrade() {
        ui.set_topic_items(ModelRc::from(Rc::new(VecModel::from(items))));
    }
}

/// Custom emoji the topic list still has to decode.
fn missing_icons(app: &App) -> Vec<String> {
    app.topics
        .iter()
        .filter_map(|topic| topic.icon_emoji_id.clone())
        .filter(|id| !app.emoji.images.contains_key(id))
        .collect()
}

fn open_topics() {
    let Some(Some((peer, generation))) = with_app(|app| app.chat.map(|peer| (peer, app.generation))) else {
        return;
    };
    with_app(|app| {
        app.topics_open = true;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_topics_open(true);
        }
    });
    load_topics(peer, generation);
}

fn pick_topic(index: i32) {
    let Some(Some((peer, generation, topic))) = with_app(|app| {
        let topic = app.topics.get(index as usize).cloned()?;
        Some((app.chat?, app.generation, topic))
    }) else {
        return;
    };
    open_topic(peer, topic.id, &topic.title, generation);
}

fn open_topic(peer: i64, topic: i32, title: &str, generation: u64) {
    with_app(|app| {
        if app.generation != generation {
            return;
        }
        app.topic = Some(topic);
        app.topic_title = title.to_owned();
        app.topics_open = false;
        app.compose = messages::Compose::None;
        app.messages.clear();
        app.message_model.clear();
        app.thumbs.clear();
        app.replies.clear();
        app.selected.clear();
        app.history_done = false;
        app.history_loading = true;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_topics_open(false);
            ui.set_topic_open(true);
            ui.set_topic_title(title.into());
            ui.set_history_loading(true);
            ui.invoke_scroll_to_bottom();
        }
    });
    run(
        async move { chats::topic_history(tg(), peer, topic, None, HISTORY_PAGE).await },
        move |page| {
            with_app(|app| {
                if app.generation != generation || app.topic != Some(topic) {
                    return;
                }
                app.history_loading = false;
                if let Some(ui) = app.ui.upgrade() {
                    ui.set_history_loading(false);
                }
                match page {
                    Ok(mut rows) => {
                        app.history_done = rows.len() < HISTORY_PAGE;
                        rows.reverse();
                        app.set_messages(rows);
                    }
                    Err(err) => app.chat_error(&err),
                }
            });
        },
    );
}

fn close_topic() {
    with_app(|app| {
        app.topic = None;
        app.topic_title.clear();
        app.topics_open = true;
        app.messages.clear();
        app.message_model.clear();
        app.history_done = true;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_topic_open(false);
            ui.set_topic_title(SharedString::new());
            ui.set_topics_open(true);
        }
    });
}

fn mark_read(peer: i64) {
    run(async move { chats::mark_read(tg(), peer).await }, move |done| {
        if done.is_ok() {
            with_app(|app| {
                if let Some(dialog) = app.dialogs.iter_mut().find(|d| d.id == peer) {
                    dialog.unread = 0;
                }
                app.refresh_dialogs();
            });
        }
    });
}

fn load_older() {
    let Some(Some((peer, before, generation, topic))) = with_app(|app| {
        if app.history_loading || app.history_done {
            return None;
        }
        let peer = app.chat?;
        let before = app.messages.first()?.id;
        app.history_loading = true;
        if let Some(ui) = app.ui.upgrade() {
            ui.set_history_loading(true);
        }
        Some((peer, before, app.generation, app.topic))
    }) else {
        return;
    };
    run(
        async move {
            match topic {
                Some(topic) => chats::topic_history(tg(), peer, topic, Some(before), HISTORY_PAGE).await,
                None => chats::history(tg(), peer, Some(before), HISTORY_PAGE).await,
            }
        },
        move |page| {
            with_app(|app| {
                if app.generation != generation {
                    return;
                }
                app.history_loading = false;
                if let Some(ui) = app.ui.upgrade() {
                    ui.set_history_loading(false);
                }
                match page {
                    Ok(rows) => {
                        app.history_done = rows.len() < HISTORY_PAGE;
                        let started = std::time::Instant::now();
                        let count = rows.len();
                        app.prepend(rows);
                        if std::env::var_os("OXIGRAM_BENCH").is_some() {
                            eprintln!("prepend {count} rows: {} ms", started.elapsed().as_millis());
                        }
                    }
                    Err(err) => app.chat_error(&err),
                }
            });
        },
    );
}

impl App {
    fn dialog(&self, peer: i64) -> Option<&DialogRow> {
        self.dialogs.iter().find(|d| d.id == peer).or_else(|| self.search.stranger(peer))
    }

    /// The header line under the chat title: presence for people, member
    /// counts for groups and channels once the profile has arrived.
    fn show_subtitle(&self, ui: &AppWindow, members: Option<(Option<i32>, Option<i32>)>) {
        let Some(dialog) = self.chat.and_then(|peer| self.dialog(peer)) else { return };
        if let Some(m) = members {
            self.members.set(Some((dialog.id, m)));
        }
        let members = members.or(self.members.get().filter(|(p, _)| *p == dialog.id).map(|(_, m)| m));
        if let Some(line) = self.typing_line(dialog) {
            ui.set_chat_subtitle(line.into());
            ui.set_chat_subtitle_accent(true);
            return;
        }
        let (text, accent) = match dialog.kind.as_str() {
            "user" if dialog.bot => (tr!("bot"), false),
            "user" => (
                format::presence_text(dialog.presence.as_ref()),
                format::is_online(dialog.presence.as_ref()),
            ),
            "channel" | "group" => {
                let (count, online) = members.unwrap_or_default();
                let channel = dialog.kind == "channel";
                let mut text = match count {
                    Some(n) if channel => trn!("{n} subscriber", "{n} subscribers", n),
                    Some(n) => trn!("{n} member", "{n} members", n),
                    None if channel => tr!("channel"),
                    None => tr!("group"),
                };
                if let Some(online) = online.filter(|n| *n > 0 && !channel) {
                    text = tr!("{}, {} online", text, format::number(i64::from(online)));
                }
                (text, false)
            }
            _ => (String::new(), false),
        };
        ui.set_chat_subtitle(text.into());
        ui.set_chat_subtitle_accent(accent);
    }

    /// "печатает…" for a dialog, naming the person in groups.
    fn typing_line(&self, dialog: &DialogRow) -> Option<String> {
        self.typing.line(dialog.id, dialog.kind == "user", |user| {
            self.messages.iter().rev().find(|m| m.sender_id == Some(user)).map(|m| m.sender_name.clone())
        })
    }

    /// The typing timer: drops expired entries, redraws what they touched.
    fn prune_typing(&mut self) {
        if self.typing.is_empty() {
            return;
        }
        let changed = self.typing.prune(std::time::Instant::now());
        for peer in changed {
            if let Some(index) = self.shown.iter().position(|p| *p == peer)
                && let Some(d) = self.dialog(peer)
            {
                self.dialog_model.set_row_data(index, self.dialog_item(d));
            }
            if self.chat == Some(peer)
                && let Some(ui) = self.ui.upgrade()
            {
                self.show_subtitle(&ui, None);
            }
        }
    }

    fn show_chat_photo(&self, ui: &AppWindow) {
        match self.chat.and_then(|peer| self.avatars.get(&peer)) {
            Some(photo) => {
                ui.set_chat_photo(photo.clone());
                ui.set_chat_has_photo(true);
            }
            None => ui.set_chat_has_photo(false),
        }
    }

    fn remember_avatar(&mut self, peer: i64, photo: slint::Image) {
        self.avatars.insert(peer, photo.clone());
        self.avatar_order.retain(|p| *p != peer);
        self.avatar_order.push_back(peer);
        while self.avatar_order.len() > AVATAR_CACHE {
            if let Some(old) = self.avatar_order.pop_front() {
                self.avatars.remove(&old);
            }
        }
        for (index, id) in self.shown.iter().enumerate() {
            if *id == peer
                && let Some(mut item) = self.dialog_model.row_data(index)
            {
                item.photo = photo.clone();
                item.has_photo = true;
                self.dialog_model.set_row_data(index, item);
            }
        }
        self.search.avatar(peer, &photo);
        if self.chat == Some(peer)
            && let Some(ui) = self.ui.upgrade()
        {
            self.show_chat_photo(&ui);
        }
    }

    fn unread_total(&self) -> i32 {
        self.dialogs
            .iter()
            .filter(|d| !d.muted && !d.archived)
            .map(|d| d.unread)
            .sum()
    }

    fn chat_error(&self, err: &UiError) {
        // The session was ended (here or from another device): back to the
        // sign-in instead of an empty window.
        if menus::session_lost(&err.code) {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_chat_error(format::error(err).into());
        }
    }

    /// Pinned chats keep their order on top; the rest go newest first.
    fn sort_dialogs(&mut self) {
        self.dialogs
            .sort_by_key(|d| (!d.pinned, if d.pinned { 0 } else { -d.last_date }));
    }

    /// The list row for a dialog.
    pub(crate) fn dialog_item(&self, d: &DialogRow) -> DialogItem {
        let title = format::dialog_title(d);
        let saved = d.kind == "saved";
        let ticks = if d.last_outgoing && (d.kind == "user" || d.kind == "group") && d.last_id > 0 {
            if d.last_id <= d.read_outbox { 2 } else { 1 }
        } else {
            0
        };
        let photo = self.avatars.get(&d.id);
        let typing = self.typing_line(d);
        // An unsent draft shows in every chat but the open one.
        let draft = if self.chat == Some(d.id) || typing.is_some() { None } else { drafts::preview(d.id) };
        DialogItem {
            initial: format::initial(&title).into(),
            title: title.into(),
            color: format::avatar_color(d.id),
            photo: photo.cloned().unwrap_or_default(),
            has_photo: photo.is_some(),
            preview: typing
                .clone()
                .or_else(|| draft.clone())
                .unwrap_or_else(|| format::preview_text(&d.last_text))
                .into(),
            typing: typing.is_some(),
            draft: draft.is_some(),
            you: typing.is_none() && draft.is_none() && d.last_outgoing && d.kind != "saved" && d.kind != "user",
            time: format::list_date(d.last_date).into(),
            unread: d.unread,
            muted: d.muted,
            pinned: d.pinned,
            online: d.kind == "user" && format::is_online(d.presence.as_ref()),
            ticks,
            saved,
            active: self.chat == Some(d.id),
            kind: d.kind.clone().into(),
            archived: d.archived,
        }
    }

    /// Composer or mute switch for the open chat, from its dialog row.
    fn show_chat_rights(&self, ui: &AppWindow) {
        let dialog = self.chat.and_then(|peer| self.dialog(peer));
        // A found public chat the user is not in: read it, or join it.
        let join = match (self.chat, dialog) {
            (Some(peer), Some(d)) if self.search.joinable(peer) => {
                if d.kind == "channel" { tr!("Subscribe") } else { tr!("Join") }
            }
            _ => String::new(),
        };
        ui.set_chat_join(join.as_str().into());
        ui.set_chat_can_post(join.is_empty() && dialog.is_none_or(|d| d.can_post));
        ui.set_chat_muted(dialog.is_some_and(|d| d.muted));
    }

    fn refresh_dialogs(&mut self) {
        let needle = &self.filter;
        let archived = self.dialogs.iter().filter(|d| d.archived).count();
        // The archive opens as its own list; search looks everywhere.
        let rows: Vec<&DialogRow> = self
            .dialogs
            .iter()
            .filter(|d| match (needle.is_empty(), self.show_archive) {
                (false, _) => true,
                (true, true) => d.archived,
                (true, false) => folders::keep(d),
            })
            .filter(|d| needle.is_empty() || format::dialog_title(d).to_lowercase().contains(needle))
            // Search shows the first 30 chats, as the WebView build does.
            .take(if needle.is_empty() { usize::MAX } else { 30 })
            .collect();
        self.shown = rows.iter().map(|d| d.id).collect();
        let items: Vec<DialogItem> = rows.into_iter().map(|d| self.dialog_item(d)).collect();
        self.dialog_model.set_vec(items);
        if let Some(ui) = self.ui.upgrade() {
            self.show_chat_rights(&ui);
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_show_archive(self.show_archive);
            folders::refresh_tabs(&ui, &self.dialogs);
            ui.set_archived_count(archived as i32);
            let n = archived as i64;
            ui.set_archived_label(trn!("{n} chat", "{n} chats", n).into());
        }
    }

    fn read_outbox(&self) -> i32 {
        self.chat
            .and_then(|peer| self.dialog(peer))
            .map(|d| d.read_outbox)
            .unwrap_or(0)
    }

    fn chat_kind(&self) -> &str {
        self.chat
            .and_then(|peer| self.dialog(peer))
            .map(|d| d.kind.as_str())
            .unwrap_or("")
    }

    /// Whether `b` continues the bubble run of `a` (same side, same day, and
    /// in groups the same sender within a few minutes).
    fn continues(&self, a: &MessageRow, b: &MessageRow) -> bool {
        if a.kind == "service" || b.kind == "service" || a.outgoing != b.outgoing {
            return false;
        }
        if !format::same_day(a.date, b.date) {
            return false;
        }
        if self.chat_kind() == "group" && !b.outgoing {
            return a.sender_id == b.sender_id && b.date - a.date <= RUN_GAP;
        }
        true
    }

    /// The row for `messages[index]`, which depends on its neighbours (date
    /// pill, sender name, gaps and the bubble tail).
    fn item(&self, index: usize) -> MessageItem {
        // An album is one bubble, drawn by its first message; the other
        // messages of the album keep empty rows.
        let album = self.album(index);
        if album.start != index {
            return MessageItem { id: self.messages[index].id, hidden: true, ..Default::default() };
        }
        let parts: Vec<&MessageRow> = self.messages[album.clone()].iter().collect();
        let wide = self.chat_kind() == "channel";
        let media = pictures::layout(&parts, wide);
        let m = &self.messages[index];
        let prev = index.checked_sub(1).map(|i| &self.messages[i]);
        let next = self.messages.get(album.end);
        let new_day = prev.is_none_or(|p| !format::same_day(p.date, m.date));
        let joined = prev.is_some_and(|p| self.continues(p, m));
        let tail = next.is_none_or(|n| !self.continues(m, n));
        let service = m.kind == "service";
        let saved = self.chat_kind() == "saved";
        let group = self.chat_kind() == "group";
        let ticks = if m.outgoing && !saved && !service && self.chat_kind() != "channel" {
            if m.id <= self.read_outbox() { 2 } else { 1 }
        } else {
            0
        };
        // The caption of an album may sit on any of its messages.
        let text = parts
            .iter()
            .map(|p| p.text.as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        // A label only for what is not drawn as a picture (files, voice…).
        let label = parts
            .iter()
            .find(|p| !pictures::has_picture(p) && !audio::is_track(p) && !files::is_file(p))
            .and_then(|p| format::kind_label(&p.kind))
            .unwrap_or_default();
        // What Oxigram cannot draw (paid posts, stories, giveaways…): say so,
        // and offer the post in Telegram instead of an empty bubble.
        let foreign = format::is_foreign(&m.kind);
        let media_link = if foreign { format::post_link(m.peer_id, m.id).unwrap_or_default() } else { String::new() };
        let label = if !foreign {
            label
        } else if media_link.is_empty() {
            oxigram_core::tr!("{} — open it in the Telegram app", label)
        } else {
            oxigram_core::tr!("{} — open in Telegram", label)
        };
        let reaction_rows = self.reaction_rows(&parts);
        // Links only for a single message's text (an album caption keeps plain text).
        let rich = if parts.len() == 1 {
            links::markdown(&m.text, &m.entities).and_then(|md| slint::StyledText::from_markdown(&md).ok())
        } else {
            None
        };
        let file_items: Vec<FileItem> = parts.iter().filter(|p| files::is_file(p)).map(|p| files::item(p)).collect();
        let track = audio::item(m);
        let has_reactions = parts.iter().any(|p| !p.reactions.is_empty());
        let quote = replies::quote(self, m);
        // Reactions go under the picture, with the time below them.
        let media_only = media.is_some() && text.is_empty() && label.is_empty() && file_items.is_empty() && !has_reactions;
        MessageItem {
            id: m.id,
            multiline: text.contains('\n'),
            text: text.into(),
            media: label.into(),
            media_link: media_link.into(),
            buttons: buttons::rows(m),
            comments_text: comments_text(m.comments).into(),
            track: track.kind,
            track_title: track.title.into(),
            track_total: track.total.into(),
            wave: track.wave.into(),
            files: ModelRc::new(VecModel::from(file_items)),
            // Links: drawn through StyledText; plain Text otherwise.
            rich: rich.clone().unwrap_or_default(),
            linked: rich.is_some(),
            reply: quote.is_some(),
            reply_who: quote.as_ref().map(|q| q.0.clone()).unwrap_or_default().into(),
            reply_text: quote.map(|q| q.1).unwrap_or_default().into(),
            cells: media.as_ref().map(|l| self.media_cells(l)).unwrap_or_default(),
            media_w: media.as_ref().map_or(0.0, |l| l.w),
            media_h: media.as_ref().map_or(0.0, |l| l.h),
            media_only,
            sticker: media.as_ref().is_some_and(|l| l.sticker),
            round: media.as_ref().is_some_and(|l| l.round),
            hidden: false,
            wide,
            reactions: reaction_rows,
            selected: self.selected.contains(&m.id),
            time: format::time(m.date).into(),
            outgoing: m.outgoing,
            ticks,
            sender: if group && !m.outgoing && !joined && !service {
                m.sender_name.clone().into()
            } else {
                SharedString::new()
            },
            sender_color: format::name_color(m.sender_id.unwrap_or_default()),
            service,
            day: if new_day { format::day(m.date).into() } else { SharedString::new() },
            edited: m.edited,
            joined,
            tail,
            can_edit: m.outgoing && m.kind == "text",
        }
    }

    /// Recomputes a row after its neighbours changed.
    fn refresh_row(&self, index: usize) {
        if index < self.messages.len() {
            self.message_model.set_row_data(index, self.item(index));
            // The album's bubble lives on its first row.
            let head = self.album(index).start;
            if head != index {
                self.message_model.set_row_data(head, self.item(head));
            }
        }
    }

    fn set_messages(&mut self, rows: Vec<MessageRow>) {
        self.messages = rows;
        let items: Vec<MessageItem> = (0..self.messages.len()).map(|i| self.item(i)).collect();
        self.message_model.set_vec(items);
        if let Some(ui) = self.ui.upgrade() {
            ui.invoke_scroll_to_bottom();
        }
    }

    /// Older messages, newest first, go above what is shown.
    fn prepend(&mut self, rows: Vec<MessageRow>) {
        let known: HashSet<i32> = self.messages.iter().map(|m| m.id).collect();
        let mut older: Vec<MessageRow> = rows.into_iter().filter(|m| !known.contains(&m.id)).collect();
        if older.is_empty() {
            return;
        }
        older.reverse();
        let count = older.len();
        older.append(&mut self.messages);
        self.messages = older;
        for i in (0..count).rev() {
            self.message_model.insert(0, self.item(i));
        }
        // The first message shown before may lose its date pill or name.
        self.refresh_row(count);
    }

    /// A message sent from here or arriving live in the open chat.
    fn add_message(&mut self, row: MessageRow) {
        if self.chat != Some(row.peer_id) {
            return;
        }
        if self.topic.is_some() && row.topic_id != self.topic {
            return;
        }
        if let Some(index) = self.messages.iter().position(|m| m.id == row.id) {
            self.messages[index] = row;
            self.refresh_row(index);
            return;
        }
        // A window around a found message has no end to append to.
        if self.search.detached {
            return;
        }
        // Decided before the list grows: own messages and further parts of
        // an album that is being followed keep the view at the bottom (an
        // album arrives one message at a time and its bubble grows).
        let same_album = row.grouped_id.is_some()
            && self.messages.last().is_some_and(|last| last.grouped_id == row.grouped_id);
        let stick = self.ui.upgrade().is_some_and(|ui| ui.get_stick_bottom()) || row.outgoing || same_album;
        self.messages.push(row);
        let index = self.messages.len() - 1;
        self.message_model.push(self.item(index));
        // The previous bubble may no longer be the last of its run.
        if let Some(prev) = index.checked_sub(1) {
            self.refresh_row(prev);
        }
        // Follow new messages only when the user is already at the bottom.
        if stick && let Some(ui) = self.ui.upgrade() {
            ui.invoke_scroll_to_bottom();
        }
    }

    /// Takes messages out of the open chat; a view at the bottom stays there
    /// (otherwise it would jump as the list gets shorter).
    pub(crate) fn remove_ids(&mut self, ids: &[i32]) {
        let stick = self.ui.upgrade().is_some_and(|ui| ui.get_stick_bottom());
        let mut removed = false;
        while let Some(index) = self.messages.iter().position(|m| ids.contains(&m.id)) {
            self.messages.remove(index);
            self.message_model.remove(index);
            if let Some(prev) = index.checked_sub(1) {
                self.refresh_row(prev);
            }
            self.refresh_row(index);
            removed = true;
        }
        self.selected.retain(|id| !ids.contains(id));
        if removed && stick && let Some(ui) = self.ui.upgrade() {
            ui.invoke_scroll_to_bottom();
        }
    }

    fn apply_events(&mut self, events: Vec<UiEvent>) {
        let mut read_now = None;
        for event in events {
            match event {
                UiEvent::Typing { peer_id, user_id, action } => {
                    if user_id != self.me_id {
                        self.typing.update(peer_id, user_id, action, std::time::Instant::now());
                        if self.chat == Some(peer_id)
                            && let Some(ui) = self.ui.upgrade()
                        {
                            self.show_subtitle(&ui, None);
                        }
                    }
                }
                UiEvent::Message { message, peer } => {
                    if let Some(sender) = message.sender_id {
                        self.typing.remove(message.peer_id, sender);
                    }
                    if self.chat == Some(message.peer_id)
                        && let Some(ui) = self.ui.upgrade()
                    {
                        self.show_subtitle(&ui, None);
                    }
                    let open = self.chat == Some(message.peer_id);
                    match self.dialogs.iter_mut().find(|d| d.id == message.peer_id) {
                        Some(dialog) => {
                            dialog.last_text = format::message_preview(&message);
                            dialog.last_date = message.date;
                            dialog.last_outgoing = message.outgoing;
                            dialog.last_id = message.id;
                            if !message.outgoing && !open {
                                dialog.unread += 1;
                            }
                        }
                        None => self.dialogs.push(DialogRow {
                            id: message.peer_id,
                            can_post: peer.kind != "channel",
                            kind: peer.kind,
                            title: peer.title,
                            last_text: format::message_preview(&message),
                            last_date: message.date,
                            last_outgoing: message.outgoing,
                            last_id: message.id,
                            unread: i32::from(!message.outgoing && !open),
                            pinned: false,
                            archived: false,
                            photo_id: peer.photo_id,
                            read_outbox: 0,
                            unread_reactions: 0,
                            muted: false,
                            contact: false,
                            bot: false,
                            presence: None,
                        }),
                    }
                    if open && !message.outgoing {
                        read_now = Some(message.peer_id);
                    }
                    self.add_message(message);
                }
                UiEvent::Edited { message } => {
                    if let Some(dialog) = self
                        .dialogs
                        .iter_mut()
                        .find(|d| d.id == message.peer_id && d.last_id == message.id)
                    {
                        dialog.last_text = format::message_preview(&message);
                    }
                    if self.messages.iter().any(|m| m.id == message.id) {
                        self.add_message(message);
                    }
                }
                UiEvent::Deleted { peer_id, ids } => {
                    let here = peer_id.is_none_or(|peer| self.chat == Some(peer));
                    if here {
                        self.remove_ids(&ids);
                    }
                }
                UiEvent::ReadOutbox { peer_id, max_id } => {
                    if let Some(dialog) = self.dialogs.iter_mut().find(|d| d.id == peer_id) {
                        dialog.read_outbox = dialog.read_outbox.max(max_id);
                    }
                    if self.chat == Some(peer_id) {
                        for index in 0..self.messages.len() {
                            let m = &self.messages[index];
                            if m.outgoing && m.id <= max_id {
                                self.refresh_row(index);
                            }
                        }
                    }
                }
                UiEvent::ReadInbox { peer_id, unread } => {
                    if let Some(dialog) = self.dialogs.iter_mut().find(|d| d.id == peer_id) {
                        dialog.unread = unread;
                    }
                }
                UiEvent::UnreadMark { peer_id, unread } => {
                    if let Some(dialog) = self.dialogs.iter_mut().find(|d| d.id == peer_id) {
                        if unread {
                            dialog.unread = dialog.unread.max(1);
                        } else if dialog.unread == 1 {
                            dialog.unread = 0;
                        }
                    }
                }
                UiEvent::Reactions { peer_id, id, reactions } => {
                    if self.chat == Some(peer_id) {
                        self.set_reactions(id, reactions);
                    }
                }
                UiEvent::Presence { peer_id, presence } => {
                    if let Some(dialog) = self.dialogs.iter_mut().find(|d| d.id == peer_id) {
                        dialog.presence = Some(presence);
                    }
                    if self.chat == Some(peer_id)
                        && let Some(ui) = self.ui.upgrade()
                    {
                        self.show_subtitle(&ui, None);
                    }
                }
                UiEvent::Upload { token, bytes } => sending::progress(&token, bytes),
                UiEvent::Connection { online } => {
                    if let Some(ui) = self.ui.upgrade() {
                        ui.set_online(online);
                    }
                }
                _ => {}
            }
        }
        self.sort_dialogs();
        self.refresh_dialogs();
        if let Some(peer) = read_now {
            mark_read(peer);
        }
    }
}

/// The Latin letter of a Ctrl shortcut typed in the Russian layout or sent as
/// a control character (Ctrl+V as U+0016), for copy, cut, paste, select all
/// and undo. None for the Latin letter itself and for other keys.
fn latin_shortcut(text: &str) -> Option<&'static str> {
    Some(match text {
        "с" | "С" | "\u{3}" => "c",
        "ч" | "Ч" | "\u{18}" => "x",
        "м" | "М" | "\u{16}" => "v",
        "ф" | "Ф" | "\u{1}" => "a",
        "я" | "\u{1a}" => "z",
        "Я" => "Z",
        _ => return None,
    })
}

#[cfg(test)]
mod latin_shortcut_tests {
    use super::latin_shortcut;

    #[test]
    fn maps_russian_layout_and_control_characters() {
        assert_eq!(latin_shortcut("м"), Some("v"));
        assert_eq!(latin_shortcut("\u{16}"), Some("v"));
        assert_eq!(latin_shortcut("с"), Some("c"));
        assert_eq!(latin_shortcut("Я"), Some("Z"));
        assert_eq!(latin_shortcut("v"), None);
        assert_eq!(latin_shortcut("б"), None);
    }
}
