//! Renders the main window offscreen to PNG files, to check the layout at a
//! given window size without a screen. Run by hand:
//! `OXIGRAM_SHOT_DIR=some/folder cargo test -p oxigram-native layout_shot -- --ignored`

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::{AppWindow, DialogItem, ProfileLine};

struct Offscreen(Rc<MinimalSoftwareWindow>);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

fn save(window: &MinimalSoftwareWindow, width: u32, height: u32, path: &std::path::Path) {
    let mut buffer = vec![Rgb565Pixel(0); (width * height) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    });
    let mut image = image::RgbImage::new(width, height);
    for (pixel, px) in image.pixels_mut().zip(&buffer) {
        let v = px.0;
        let r = ((v >> 11) & 0x1f) as u8;
        let g = ((v >> 5) & 0x3f) as u8;
        let b = (v & 0x1f) as u8;
        *pixel = image::Rgb([r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2]);
    }
    image.save(path).unwrap();
}

#[test]
#[ignore]
fn layout_shot() {
    let Ok(dir) = std::env::var("OXIGRAM_SHOT_DIR") else { return };
    let dir = std::path::PathBuf::from(dir);
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.set_stage("ready".into());
    let dialogs: Vec<DialogItem> = (0..14)
        .map(|i| DialogItem {
            title: format!("Chat {i} 🇩🇪🇫🇷 with a long name").into(),
            initial: "C".into(),
            preview: "The last message in this chat, fairly long so that it gets truncated".into(),
            time: "12:34".into(),
            unread: if i % 3 == 0 { 5 } else { 0 },
            kind: "channel".into(),
            active: i == 2,
            ..Default::default()
        })
        .collect();
    ui.set_dialogs(ModelRc::new(VecModel::from(dialogs)));
    ui.set_chat_open(true);
    ui.set_chat_title("Channel 🇷🇺🇺🇦🇰🇿🏴󠁧󠁢󠁥󠁮󠁧󠁿 with a long name 😀".into());
    let lines = vec![
        ProfileLine {
            value: "Channel description: https://example.com/a/very/long/link/without/any/spaces/that/goes/on/and/on/and/on/forever".into(),
            label: "Description".into(),
        },
        ProfileLine { value: "@channel_name".into(), label: "Link".into() },
    ];
    ui.set_info_lines(ModelRc::new(VecModel::from(lines)));
    let messages: Vec<crate::MessageItem> = (0..4)
        .map(|i| crate::MessageItem {
            id: i,
            text: "A channel post with fairly long text so that it wraps onto several lines and fills the whole column width. https://example.com/very/long/link/without/spaces/at/all/that/goes/on/and/on".into(),
            time: "12:00".into(),
            wide: i % 2 == 0,
            outgoing: i == 3,
            multiline: true,
            tail: true,
            media: if i == 5 { "Photo".into() } else { "".into() },
            linked: i == 1,
            buttons: if i == 2 {
                let key = |text: &str, link: bool, row: i32, col: i32| crate::KeyButton { text: text.into(), link, row, col };
                ModelRc::new(VecModel::from(vec![
                    crate::ButtonRow { items: ModelRc::new(VecModel::from(vec![key("Купить", false, 0, 0), key("Сайт", true, 0, 1)])) },
                    crate::ButtonRow { items: ModelRc::new(VecModel::from(vec![key("Подробнее о магазине", false, 1, 0)])) },
                ]))
            } else {
                Default::default()
            },
            reply: i >= 2,
            reply_who: if i == 3 { "You".into() } else { "John Smith".into() },
            reply_text: "The text of the message being replied to, long enough to be truncated to one line".into(),
            rich: if i == 1 {
                let text = "Links: example.com/page and a hidden link, @durov, #news 😀\n\n  Second line (1. not a list) *not bold*";
                let entities = [
                    oxigram_core::store::chats::Entity { kind: "url".into(), offset: 8, length: 16, target: None },
                    oxigram_core::store::chats::Entity {
                        kind: "textUrl".into(),
                        offset: 27,
                        length: 14,
                        target: Some("https://x.org".into()),
                    },
                    oxigram_core::store::chats::Entity { kind: "mention".into(), offset: 43, length: 6, target: None },
                    oxigram_core::store::chats::Entity { kind: "hashtag".into(), offset: 51, length: 8, target: None },
                ];
                slint::StyledText::from_markdown(&crate::links::markdown(text, &entities).unwrap()).unwrap()
            } else {
                Default::default()
            },
            media_w: if i == 5 { 600.0 } else { 0.0 },
            media_h: if i == 5 { 400.0 } else { 0.0 },
            files: if i >= 2 {
                ModelRc::new(VecModel::from(
                    (0..3)
                        .map(|state| crate::FileItem {
                            id: i,
                            name: format!("Quarterly report number {state} with a long name.pdf").into(),
                            ext: "PDF".into(),
                            size: if state == 1 { "0.4 MB of 1.2 MB".into() } else { "1.2 MB".into() },
                            state,
                            progress: 0.33,
                        })
                        .collect::<Vec<_>>(),
                ))
            } else {
                Default::default()
            },
            ..Default::default()
        })
        .collect();
    ui.set_messages(ModelRc::new(VecModel::from(messages)));
    ui.set_info_title("Channel".into());
    for (width, height) in [(1690u32, 900u32), (1352, 800), (1170, 800), (1100, 800), (900, 700)] {
        window.set_size(slint::PhysicalSize::new(width, height));
        ui.show().unwrap();
        for open in [false, true] {
            ui.set_info_open(open);
            save(&window, width, height, &dir.join(format!("shot-{width}-{}.png", if open { "info" } else { "plain" })));
        }
    }
    // The proxy window, with a server and with the WebSocket bypass.
    window.set_size(slint::PhysicalSize::new(1100, 800));
    ui.set_info_open(false);
    ui.set_proxy_open(true);
    ui.set_proxy_enabled(true);
    for kind in ["socks5", "websocket"] {
        ui.set_proxy_kind(kind.into());
        save(&window, 1100, 800, &dir.join(format!("shot-proxy-{kind}.png")));
    }
    // A newer version: the badge, then "О программе" while downloading.
    ui.set_proxy_open(false);
    ui.set_update_version("0.3.1".into());
    save(&window, 1100, 800, &dir.join("shot-update-badge.png"));
    ui.set_version("0.3.0".into());
    ui.set_update_busy(true);
    ui.set_update_progress(0.4);
    ui.set_update_status("Downloading… 2.8 MB of 7.1 MB".into());
    ui.set_settings_page("about".into());
    ui.set_settings_open(true);
    save(&window, 1100, 800, &dir.join("shot-update-about.png"));
    // "Код-пароль", off and on.
    ui.set_settings_page("passcode".into());
    ui.set_auto_lock_text("через 1 ч".into());
    for on in [false, true] {
        ui.set_passcode_on(on);
        save(&window, 1100, 800, &dir.join(format!("shot-passcode-{on}.png")));
    }
    // The lock screen.
    ui.set_settings_open(false);
    ui.set_stage("locked".into());
    ui.set_error("Неверный код-пароль".into());
    save(&window, 1100, 800, &dir.join("shot-locked.png"));

    // Sign-in: the code step, the QR step.
    ui.set_stage("code".into());
    ui.set_code_hint("The code was sent to the Telegram app on your other device (+380 67 123 45 67): look in the chat with Telegram.".into());
    ui.set_resend_text("Send the code by SMS (0:42)".into());
    save(&window, 1100, 800, &dir.join("shot-code.png"));
    let (path, size) = oxigram_core::login::qr_path("tg://login?token=AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA").unwrap();
    ui.set_qr_path(path.into());
    ui.set_qr_size(size);
    ui.set_stage("qr".into());
    save(&window, 1100, 800, &dir.join("shot-qr.png"));

    // Each language: the chat with the profile, the general settings, the
    // phone step with the country list.
    ui.set_error("".into());
    for code in ["en", "ru", "uk"] {
        crate::i18n::set_language(code);
        ui.set_stage("ready".into());
        ui.set_info_open(true);
        save(&window, 1100, 800, &dir.join(format!("shot-lang-{code}-chat.png")));
        ui.set_info_open(false);
        ui.set_settings_page("general".into());
        ui.set_settings_open(true);
        save(&window, 1100, 800, &dir.join(format!("shot-lang-{code}-settings.png")));
        ui.set_settings_page("main".into());
        save(&window, 1100, 800, &dir.join(format!("shot-lang-{code}-settings-main.png")));
        ui.set_settings_open(false);
        ui.set_stage("phone".into());
        crate::phone::set_country(&ui, crate::countries::by_iso("UA"));
        save(&window, 1100, 800, &dir.join(format!("shot-lang-{code}-phone.png")));
        let items: Vec<crate::CountryItem> = crate::countries::all()
            .iter()
            .take(12)
            .map(|c| crate::CountryItem {
                flag: crate::countries::flag(c.iso).into(),
                name: oxigram_core::i18n::tr(c.name).into(),
                dial: format!("+{}", c.dial).into(),
            })
            .collect();
        ui.set_country_items(ModelRc::new(VecModel::from(items)));
        ui.set_country_open(true);
        save(&window, 1100, 800, &dir.join(format!("shot-lang-{code}-countries.png")));
        ui.set_country_open(false);
    }
}
