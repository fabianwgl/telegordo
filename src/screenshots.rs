//! README screenshots: the real UI, drawn from made-up chats.
//!
//! `readme_frames` builds a few scenes and saves, for each one, the exact bytes
//! the app writes to the terminal, inline images included.
//! `scripts/screenshots.sh` runs it inside a Ghostty window, replays each frame
//! there and captures the window. Nothing here talks to Telegram.

use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use chrono::{Local, TimeZone};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use tdlib_rs::enums::{MessageContent, MessageOrigin, MessageSender, MessageTopic, TextEntityType};
use tdlib_rs::types::{
    ChatPosition, Document, File, FormattedText, ForumTopicInfo, Message, MessageChatAddMembers,
    MessageDocument, MessageForumTopicCreated, MessageForwardInfo, MessageOriginChannel,
    MessageSenderUser, MessageText, MessageTopicForum, MessageVoiceNote, TextEntity,
    TextEntityTypePreCode, VoiceNote,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::app::{App, AppEvent, Auth, Focus, Modal};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::testutil;
use crate::tg::store::{ChatInfo, ChatKind, ListKey, Topic, Typing};
use crate::tg::{KeyStore, Params};
use crate::ui::chats_tree::NodeId;
use crate::ui::composer::Mode;
use crate::ui::image::{self, Entry, Images};
use crate::ui::picker::ChatPicker;

// Users. The sender name color comes from the id, so these get different colors.
const ME: i64 = 1;
const LEO: i64 = 3;
const MAYA: i64 = 4;
const SAM: i64 = 5;
const PRIYA: i64 = 6;
const DANA: i64 = 7;
const TOMAS: i64 = 9;

// Chats.
const HIKE: i64 = 100;
const DESIGN: i64 = 101;
const RUST: i64 = 102;
const TRAIL: i64 = 106;
const RELEASES: i64 = 111;
const WORK: i32 = 1;
const PERSONAL: i32 = 2;
const HELP: i32 = 3;
const PHOTO: i32 = 1;

#[test]
#[ignore = "writes README screenshot frames; run scripts/screenshots.sh"]
fn readme_frames() {
    let dir = std::env::var_os("TELEGORDO_SHOTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/screenshots"));
    std::fs::create_dir_all(&dir).unwrap();
    let (w, h, font) = terminal_size();
    let photo = dir.join("summit.png");
    summit_photo().save(&photo).unwrap();
    let photo = photo.to_string_lossy();

    // Each step changes the app, then draws a frame.
    type Step = fn(&mut App);
    let scenes: [(&str, &[Step]); 3] = [
        ("chat", &[chat_scene]),
        ("topics", &[topic_scene]),
        ("picker", &[chat_scene, picker_scene]),
    ];
    let mut names = Vec::new();
    for (name, steps) in scenes {
        let (mut app, _rx) = demo_app(&photo, font);
        let mut rec = Recorder::new(w, h);
        for step in steps {
            step(&mut app);
            rec.draw(&mut app);
        }
        std::fs::write(dir.join(format!("{name}.ans")), &*rec.tape.0.borrow()).unwrap();
        names.push(name);
    }
    std::fs::write(dir.join("frames.txt"), names.join("\n") + "\n").unwrap();
}

/// "Weekend hike" with Sam's file selected and a reply to it drafted.
fn chat_scene(app: &mut App) {
    open(app, NodeId::Chat(ListKey::Main, HIKE), Focus::Messages);
    app.msgs.selected = Some(70);
    let draft = "Thanks! Loading it on my watch **tonight**";
    app.composer.set_text(draft);
    app.composer.mode = Mode::Reply(70);
}

/// The forum expanded in the tree, showing its "Help" topic.
fn topic_scene(app: &mut App) {
    app.tree.expanded.insert(NodeId::Chat(ListKey::Main, RUST));
    open(app, NodeId::Topic(ListKey::Main, RUST, HELP), Focus::Tree);
}

/// The `ctrl+k` quick switcher with a query typed.
fn picker_scene(app: &mut App) {
    let mut p = ChatPicker::new(&app.store);
    p.query = "ru".into();
    p.refilter();
    app.modal = Modal::Picker(p);
}

fn open(app: &mut App, node: NodeId, focus: Focus) {
    app.tree.selected = Some(node);
    app.current = node.view();
    app.focus = focus;
}

/// Cells and pixels per cell of the terminal this runs in (the screenshot
/// window), or a 120×34 grid of 16×34-pixel cells when there is none.
fn terminal_size() -> (u16, u16, FontSize) {
    match crossterm::terminal::window_size() {
        Ok(s) if s.columns > 0 && s.rows > 0 && s.width > 0 && s.height > 0 => (
            s.columns,
            s.rows,
            FontSize::new(s.width / s.columns, s.height / s.rows),
        ),
        _ => (120, 34, FontSize::new(16, 34)),
    }
}

/// A terminal that keeps every byte the app writes to it.
struct Recorder {
    term: Terminal<CrosstermBackend<Tape>>,
    tape: Tape,
    area: Rect,
}

/// The recorded bytes, shared with the backend that writes them.
#[derive(Clone, Default)]
struct Tape(Rc<RefCell<Vec<u8>>>);

impl Write for Tape {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Recorder {
    fn new(w: u16, h: u16) -> Self {
        let area = Rect::new(0, 0, w, h);
        let tape = Tape::default();
        let options = TerminalOptions {
            viewport: Viewport::Fixed(area),
        };
        let term = Terminal::with_options(CrosstermBackend::new(tape.clone()), options).unwrap();
        Self { term, tape, area }
    }

    /// Draws a frame with its images already decoded, as the app does once
    /// they have loaded.
    fn draw(&mut self, app: &mut App) {
        let mut dry = Terminal::new(TestBackend::new(self.area.width, self.area.height)).unwrap();
        app.wanted.clear();
        dry.draw(|f| crate::ui::draw(f, app)).unwrap();
        let picker = app.images.picker.clone().expect("images are on");
        for w in std::mem::take(&mut app.wanted) {
            if app.images.protocol(w.file_id).is_none() {
                let path = &app.store.files[&w.file_id].local.path;
                let proto = image::encode(&picker, path, w.cols, w.rows).unwrap();
                app.images.entries.insert(w.file_id, Entry::Ready(proto));
            }
        }
        self.term.draw(|f| crate::ui::draw(f, app)).unwrap();
    }
}

/// Logged-in app with the demo chats. Keep the receiver alive while drawing.
fn demo_app(photo: &str, font: FontSize) -> (App, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    let cfg = Config::default();
    // The screenshots are taken in Ghostty, which has the kitty keyboard protocol.
    let (keys, _) = Keymap::build(&cfg.keys, true);
    // The only constructor that takes a known cell size instead of asking the terminal.
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(ProtocolType::Kitty);
    let images = Images {
        picker: Some(picker),
        max_cols: cfg.images.max_width,
        max_rows: cfg.images.max_height,
        entries: Default::default(),
    };
    let params = Params {
        use_test_dc: true,
        database_dir: "/nonexistent".into(),
        files_dir: "/nonexistent".into(),
        encryption_key: String::new(),
        api_id: 0,
        api_hash: String::new(),
        key_store: KeyStore::File("/nonexistent".into()),
    };
    let mut app = App::new(cfg, keys, testutil::shared_tg(), params, images, tx);
    app.auth = Auth::Ready;
    add_chats(&mut app);
    add_hike(&mut app, photo);
    add_help_topic(&mut app);
    (app, rx)
}

fn add_chats(app: &mut App) {
    use ListKey::{Archive, Folder, Main};
    let s = &mut app.store;
    s.me = Some(ME);
    for (id, name) in [
        (ME, "Me"),
        (LEO, "Leo"),
        (MAYA, "Maya"),
        (SAM, "Sam"),
        (PRIYA, "Priya"),
        (DANA, "Dana"),
        (TOMAS, "Tomás"),
    ] {
        s.users.insert(id, testutil::user(id, name));
    }
    s.folders = vec![(WORK, "Work".into()), (PERSONAL, "Personal".into())];

    let group = ChatKind::BasicGroup;
    let forum = ChatKind::Supergroup {
        id: 900,
        channel: false,
    };
    let channel = |id| ChatKind::Supergroup { id, channel: true };
    let personal = || vec![Main, Folder(PERSONAL)];
    let work = || vec![Main, Folder(WORK)];
    // Telegram's order, most recent first: id, title, kind, chat lists, unread count.
    let chats = [
        (HIKE, "Weekend hike", group, personal(), 2),
        (DESIGN, "Design review", group, work(), 4),
        (RUST, "Rust meetup", forum, vec![Main], 3),
        (103, "Family", group, personal(), 1),
        (104, "Jonas", ChatKind::Private(20), personal(), 1),
        (105, "Dana", ChatKind::Private(DANA), personal(), 0),
        (TRAIL, "Trail news", channel(901), vec![Main], 12),
        (107, "Book club", group, personal(), 0),
        (108, "Coffee crew", group, work(), 0),
        (109, "Climbing gym", group, vec![Main], 0),
        (110, "Ana", ChatKind::Private(21), personal(), 0),
        (RELEASES, "Release notes", channel(902), work(), 5),
        (112, "Saved Messages", ChatKind::Private(ME), vec![Main], 0),
        (113, "Board games", group, personal(), 0),
        (114, "Standup", group, work(), 0),
        (115, "Photo walks", group, vec![Main], 0),
        (116, "Neighbours", group, vec![Main], 0),
        (117, "Old flatmates", group, vec![Archive], 0),
    ];
    let n = chats.len() as i64;
    for (i, (id, title, kind, lists, unread)) in chats.into_iter().enumerate() {
        let positions = lists
            .into_iter()
            .map(|l| ChatPosition {
                list: l.to_list(),
                order: n - i as i64,
                is_pinned: false,
                source: None,
            })
            .collect();
        let info = ChatInfo {
            id,
            title: title.into(),
            kind,
            positions,
            unread,
            unread_mentions: 0,
            marked_unread: false,
            last_read_inbox: 0,
            mute_for: 0,
        };
        s.chats.insert(id, info);
    }
    s.chats.get_mut(&DESIGN).unwrap().unread_mentions = 1;
    for muted in [TRAIL, RELEASES] {
        s.chats.get_mut(&muted).unwrap().mute_for = i32::MAX;
    }
    s.forum_supergroups.insert(900);
    s.topics.insert(
        RUST,
        vec![
            topic(1, "General", 0, 4),
            topic(HELP, "Help", 2, 3),
            topic(2, "Showcase", 1, 2),
            topic(4, "Jobs", 0, 1),
        ],
    );
}

fn topic(id: i32, name: &str, unread: i32, order: i64) -> Topic {
    Topic {
        info: ForumTopicInfo {
            chat_id: RUST,
            forum_topic_id: id,
            name: name.into(),
            icon: Default::default(),
            creation_date: 0,
            creator_id: MessageSender::User(MessageSenderUser { user_id: PRIYA }),
            is_general: id == 1,
            is_outgoing: false,
            is_closed: false,
            is_hidden: false,
            is_name_implicit: false,
        },
        unread,
        order,
        is_pinned: id == 1,
        mute_for: 0,
    }
}

fn add_hike(app: &mut App, photo: &str) {
    use TextEntityType as T;
    let plain = |s: &str| text(Rich::default().plain(s));
    let added = add_members(&[MAYA, SAM]);
    msg(app, HIKE, 10, LEO, at(19, 18, 2), None, added);
    let ask = "Who's up for the ridge trail on Saturday? Leaving at 7 from the station.";
    msg(app, HIKE, 20, LEO, at(19, 18, 12), None, plain(ask));
    let coffee = Rich::default()
        .with("Me!", T::Bold)
        .plain(" Bringing the ")
        .with("good", T::Italic)
        .plain(" coffee");
    msg(app, HIKE, 30, MAYA, at(19, 18, 15), None, text(coffee));
    let snacks = plain("Count me in! I'll bring snacks");
    msg(app, HIKE, 35, ME, at(19, 18, 18), None, snacks);
    let news = "North trail is open again after the storm. Expect mud near the lake.";
    msg(app, HIKE, 40, SAM, at(19, 18, 21), None, plain(news)).forward_info =
        Some(MessageForwardInfo {
            origin: MessageOrigin::Channel(MessageOriginChannel {
                chat_id: TRAIL,
                message_id: 1,
                author_signature: String::new(),
            }),
            date: at(19, 17, 40),
            source: None,
            public_service_announcement_type: String::new(),
        });

    let file = testutil::file(PHOTO, photo, true);
    app.store.files.insert(PHOTO, file);
    let mut summit: MessageContent =
        serde_json::from_value(testutil::photo_content(PHOTO, photo, 1280, 408)).unwrap();
    if let MessageContent::MessagePhoto(p) = &mut summit {
        p.caption = Rich::default()
            .plain("Made it to the top! The view is unreal")
            .0;
    }
    msg(app, HIKE, 50, LEO, at(20, 7, 2), None, summit);
    let jealous = Rich::default()
        .plain("Jealous. Save me a seat at the summit café. ")
        .with("I'm already halfway up", T::Spoiler);
    msg(app, HIKE, 60, MAYA, at(20, 7, 5), Some(50), text(jealous));
    let caption = Rich::default()
        .plain("Route for next time, with pace notes: ")
        .with("https://example.com/ridge-loop", T::Url);
    let route = document("ridge-loop.gpx", 48_000, caption);
    msg(app, HIKE, 70, SAM, at(20, 7, 9), None, route).edit_date = at(20, 7, 10);
    let voice = MessageContent::MessageVoiceNote(MessageVoiceNote {
        voice_note: VoiceNote {
            duration: 12,
            ..Default::default()
        },
        ..Default::default()
    });
    msg(app, HIKE, 80, LEO, at(20, 7, 11), None, voice);
    let typing = Typing {
        sender: MAYA,
        topic: None,
        at: Instant::now(),
    };
    app.store.typing.insert(HIKE, vec![typing]);
}

fn add_help_topic(app: &mut App) {
    use TextEntityType as T;
    let created = MessageContent::MessageForumTopicCreated(MessageForumTopicCreated {
        name: "Help".into(),
        ..Default::default()
    });
    let beginners = Rich::default().plain("Is there a beginner track at the next meetup?");
    let laptop = Rich::default()
        .plain("Yes! Bring a laptop with ")
        .with("rustup", T::Code)
        .plain(" installed");
    let pair = Rich::default().plain("I can pair with anyone stuck on ownership");
    let thanks_all = Rich::default().plain("Thanks all!");
    let slides = document(
        "meetup-slides.pdf",
        2_100_000,
        Rich::default().plain("Slides from last time, for a head start"),
    );
    let code =
        "fn longest(a: &str, b: &str) -> &str {\n    if a.len() > b.len() { a } else { b }\n}";
    let question = Rich::default()
        .plain("Why does this not compile? The borrow checker and I are not friends today\n")
        .with(
            code,
            T::PreCode(TextEntityTypePreCode {
                language: "rust".into(),
            }),
        );
    let answer = Rich::default()
        .plain("It needs a lifetime: ")
        .with("fn longest<'a>(a: &'a str, b: &'a str) -> &'a str", T::Code)
        .plain("\nThe result can only live as long as both inputs. More in ")
        .with("https://example.com/lifetimes", T::Url);
    let thanks = Rich::default()
        .plain("That fixed it, thanks ")
        .with("@tomas", T::Mention)
        .plain("!");
    let tip = Rich::default()
        .plain("Tip: ")
        .with("cargo clippy", T::Code)
        .plain(" explains most borrow errors with a ")
        .with("help:", T::Bold)
        .plain(" line");
    let faq = Rich::default()
        .plain("Adding this to the ")
        .with("meetup FAQ", T::Italic)
        .plain(". ")
        .with("Next week", T::Strikethrough)
        .plain(" Thursday we look at ")
        .with("async", T::Underline);
    let messages = [
        (200, PRIYA, at(19, 20, 40), None, created),
        (201, DANA, at(19, 20, 47), None, text(beginners)),
        (202, PRIYA, at(19, 20, 52), None, text(laptop)),
        (203, TOMAS, at(19, 21, 5), None, text(pair)),
        (204, DANA, at(19, 21, 8), None, slides),
        (205, DANA, at(19, 21, 9), None, text(thanks_all)),
        (207, PRIYA, at(20, 10, 14), None, text(question)),
        (208, TOMAS, at(20, 10, 17), Some(207), text(answer)),
        (209, PRIYA, at(20, 10, 19), None, text(thanks)),
        (210, SAM, at(20, 10, 31), None, text(tip)),
        (211, ME, at(20, 10, 40), None, text(faq)),
    ];
    for (id, user, date, reply, content) in messages {
        let topic = MessageTopic::Forum(MessageTopicForum {
            forum_topic_id: HELP,
        });
        msg(app, RUST, id, user, date, reply, content).topic_id = Some(topic);
    }
}

/// Adds a message from `user`; user 1 is "Me".
fn msg(
    app: &mut App,
    chat: i64,
    id: i64,
    user: i64,
    date: i32,
    reply: Option<i64>,
    content: MessageContent,
) -> &mut Message {
    let mut m = testutil::message(id, chat, user, date, testutil::text_content(""), reply);
    m.content = content;
    app.store.insert_message(m);
    app.store
        .messages
        .get_mut(&chat)
        .and_then(|c| c.get_mut(&id))
        .unwrap()
}

/// Unix time of a local date and time in November 2026.
fn at(day: u32, hour: u32, min: u32) -> i32 {
    Local
        .with_ymd_and_hms(2026, 11, day, hour, min, 0)
        .single()
        .unwrap()
        .timestamp() as i32
}

fn text(r: Rich) -> MessageContent {
    MessageContent::MessageText(MessageText {
        text: r.0,
        link_preview: None,
        link_preview_options: None,
    })
}

fn document(name: &str, size: i64, caption: Rich) -> MessageContent {
    MessageContent::MessageDocument(MessageDocument {
        document: Document {
            file_name: name.into(),
            mime_type: String::new(),
            minithumbnail: None,
            thumbnail: None,
            document: File {
                size,
                ..Default::default()
            },
        },
        caption: caption.0,
    })
}

fn add_members(ids: &[i64]) -> MessageContent {
    MessageContent::MessageChatAddMembers(MessageChatAddMembers {
        member_user_ids: ids.to_vec(),
    })
}

/// Formatted text built piece by piece, so the UTF-16 entity offsets stay right.
#[derive(Default)]
struct Rich(FormattedText);

impl Rich {
    fn plain(mut self, s: &str) -> Self {
        self.0.text.push_str(s);
        self
    }

    fn with(mut self, s: &str, r#type: TextEntityType) -> Self {
        self.0.entities.push(TextEntity {
            offset: self.0.text.encode_utf16().count() as i32,
            length: s.encode_utf16().count() as i32,
            r#type,
        });
        self.plain(s)
    }
}

/// A made-up mountain panorama at sunrise, 1280×408 like a wide phone photo.
fn summit_photo() -> ::image::RgbImage {
    let (w, h) = (1280.0f32, 408.0f32);
    let sun = (0.6f32, 0.4f32);
    // Far to near: foot of the range (0 = top), peak height, color at the top and at the foot.
    let ranges = [
        (0.62f32, 0.30f32, [186u8, 136, 164], [140, 104, 146]),
        (0.74, 0.28, [124, 90, 136], [88, 66, 112]),
        (0.86, 0.22, [72, 56, 98], [46, 38, 72]),
        (1.00, 0.16, [34, 28, 54], [16, 14, 30]),
    ];
    ::image::RgbImage::from_fn(w as u32, h as u32, |x, y| {
        let (fx, fy) = (x as f32 / w, y as f32 / h);
        // Sky: deep blue overhead, warm at the horizon, with the sun's glow.
        let mut c = mix(
            [28, 40, 96],
            [255, 172, 112],
            (fy / 0.66).clamp(0.0, 1.0).powf(1.6),
        );
        let d = ((fx - sun.0) * w / h).hypot(fy - sun.1);
        c = mix(c, [255, 224, 170], (1.0 - d / 0.5).clamp(0.0, 1.0).powi(3));
        if d < 0.06 {
            c = [255, 244, 220];
        }
        for (i, &(foot, peak, top, bottom)) in ranges.iter().enumerate() {
            let k = i as f32 * 2.3 + 0.7;
            // Triangle waves at unrelated frequencies: straight slopes, uneven peaks.
            let tri = |f: f32, p: f32| 1.0 - 2.0 * ((fx * f + p).fract() - 0.5).abs();
            let ridge = foot
                - peak
                    * (0.55 * tri(2.3 + k, k)
                        + 0.3 * tri(5.9 + k, 2.0 * k)
                        + 0.15 * tri(13.7, 3.0 * k));
            if fy > ridge {
                // Shade by height alone, so steep slopes do not streak.
                c = mix(
                    top,
                    bottom,
                    ((fy - foot + peak) / (peak + 0.1)).clamp(0.0, 1.0),
                );
            }
        }
        ::image::Rgb(c)
    })
}

fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
}
