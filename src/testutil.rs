//! Test fixtures: TDLib objects built from JSON, and an `App` with a fake store.

use serde_json::{Value, json};
use tdlib_rs::types::{ChatPosition, File, Message, User};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::app::{App, AppEvent, Auth};
use crate::config::Config;
use crate::keymap::Keymap;
use crate::tg::store::{ChatInfo, ChatKind, ListKey};
use crate::tg::{KeyStore, Params, Tg};
use crate::ui::image::Images;

pub fn text_content(text: &str) -> Value {
    json!({"@type": "messageText",
           "text": {"text": text, "entities": [{"offset": 0, "length": 5, "type": {"@type": "textEntityTypeBold"}}]},
           "link_preview": null, "link_preview_options": null})
}

pub fn file_json(id: i32, path: &str, done: bool) -> Value {
    json!({"id": id, "size": 100, "expected_size": 100,
           "local": {"path": path, "can_be_downloaded": true, "can_be_deleted": true,
                     "is_downloading_active": false, "is_downloading_completed": done,
                     "download_offset": 0, "downloaded_prefix_size": 100, "downloaded_size": 100},
           "remote": {"id": "r", "unique_id": "u", "is_uploading_active": false,
                      "is_uploading_completed": true, "uploaded_size": 100}})
}

pub fn file(id: i32, path: &str, done: bool) -> File {
    serde_json::from_value(file_json(id, path, done)).expect("file json")
}

pub fn photo_content(file_id: i32, path: &str, w: i32, h: i32) -> Value {
    json!({"@type": "messagePhoto",
           "photo": {"has_stickers": false, "minithumbnail": null,
                     "sizes": [{"type": "m", "photo": file_json(file_id, path, true), "width": w, "height": h,
                                "progressive_sizes": []}]},
           "caption": {"text": "", "entities": []},
           "show_caption_above_media": false, "has_spoiler": false, "is_secret": false})
}

pub fn message(
    id: i64,
    chat_id: i64,
    user: i64,
    date: i32,
    content: Value,
    reply_to: Option<i64>,
) -> Message {
    let reply = reply_to.map(|r| {
        json!({"@type": "messageReplyToMessage", "chat_id": chat_id, "message_id": r, "quote": null,
               "checklist_task_id": 0, "origin": null, "origin_send_date": 0, "content": null})
    });
    serde_json::from_value(json!({
        "id": id, "sender_id": {"@type": "messageSenderUser", "user_id": user}, "chat_id": chat_id,
        "sending_state": null, "scheduling_state": null, "is_outgoing": user == 1, "is_pinned": false,
        "is_from_offline": false, "can_be_saved": true, "has_timestamped_media": false,
        "is_channel_post": false, "is_paid_star_suggested_post": false, "is_paid_ton_suggested_post": false,
        "contains_unread_mention": false, "date": date, "edit_date": 0, "forward_info": null,
        "import_info": null, "interaction_info": null, "unread_reactions": [], "fact_check": null,
        "suggested_post_info": null, "reply_to": reply, "topic_id": null, "self_destruct_type": null,
        "self_destruct_in": 0.0, "auto_delete_in": 0.0, "via_bot_user_id": 0,
        "sender_business_bot_user_id": 0, "sender_boost_count": 0, "paid_message_star_count": 0,
        "author_signature": "", "media_album_id": "0", "effect_id": "0", "restriction_info": null,
        "summary_language_code": "", "reply_markup": null, "content": content
    }))
    .expect("message json")
}

pub fn user(id: i64, name: &str) -> User {
    serde_json::from_value(json!({"id": id, "first_name": name, "last_name": "", "usernames": null, "phone_number": "",
        "status": {"@type": "userStatusEmpty"}, "profile_photo": null, "accent_color_id": 0,
        "background_custom_emoji_id": "0", "upgraded_gift_colors": null, "profile_accent_color_id": 0,
        "profile_background_custom_emoji_id": "0", "emoji_status": null, "is_contact": false,
        "is_mutual_contact": false, "is_close_friend": false, "verification_status": null,
        "is_premium": false, "is_support": false, "restriction_info": null, "active_story_state": null,
        "restricts_new_chats": false, "paid_message_star_count": 0, "have_access": true,
        "type": {"@type": "userTypeRegular"}, "language_code": "", "added_to_attachment_menu": false}))
    .expect("user json")
}

pub fn chat(app: &mut App, id: i64, title: &str, order: i64, unread: i32, mute_for: i32) {
    app.store.chats.insert(
        id,
        ChatInfo {
            id,
            title: title.into(),
            kind: ChatKind::BasicGroup,
            positions: vec![ChatPosition {
                list: ListKey::Main.to_list(),
                order,
                is_pinned: false,
                source: None,
            }],
            unread,
            unread_mentions: 0,
            marked_unread: false,
            last_read_inbox: 0,
            mute_for,
        },
    );
}

pub const BASE_DATE: i32 = 1_795_000_000;

/// TDLib allows one `receive` thread per process, so tests share one client.
pub fn shared_tg() -> Tg {
    static TG: std::sync::OnceLock<Tg> = std::sync::OnceLock::new();
    TG.get_or_init(|| {
        let (tx, rx) = unbounded_channel();
        // Updates are not used by tests; keep the receiver alive forever.
        Box::leak(Box::new(rx));
        Tg::start(tx)
    })
    .clone()
}

/// Logged-in app with three chats; "Family" (10) has three messages.
/// Keep the receiver alive for the test's duration.
pub fn app(images: Images, enhanced: bool) -> (App, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    let cfg = Config::default();
    let (keys, _) = Keymap::build(&cfg.keys, enhanced);
    let params = Params {
        use_test_dc: true,
        database_dir: "/nonexistent".into(),
        files_dir: "/nonexistent".into(),
        encryption_key: String::new(),
        api_id: 0,
        api_hash: String::new(),
        key_store: KeyStore::File("/nonexistent".into()),
    };
    let mut app = App::new(cfg, keys, shared_tg(), params, images, tx);
    app.auth = Auth::Ready;
    chat(&mut app, 10, "Family", 30, 3, 0);
    chat(&mut app, 11, "Muted news", 20, 9, 3600);
    chat(&mut app, 12, "Saved Messages", 10, 0, 0);
    app.store.users.insert(1, user(1, "Me"));
    app.store.users.insert(2, user(2, "Alice"));
    let b = BASE_DATE;
    app.store.insert_message(message(
        100,
        10,
        2,
        b,
        text_content("Hello there, how are you?"),
        None,
    ));
    app.store.insert_message(message(
        101,
        10,
        2,
        b + 30,
        text_content("Second line from Alice"),
        None,
    ));
    app.store.insert_message(message(
        102,
        10,
        1,
        b + 90,
        text_content("Great thanks"),
        Some(100),
    ));
    (app, rx)
}
