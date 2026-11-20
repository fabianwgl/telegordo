//! In-memory model of chats, folders, users and messages, kept current from
//! TDLib updates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use tdlib_rs::enums::{
    ChatAction, ChatList, ChatType, MessageSender, MessageTopic, NotificationSettingsScope, Update,
};
use tdlib_rs::types::{ChatPosition, File, ForumTopic, ForumTopicInfo, Message, User};

use super::View;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ListKey {
    Main,
    Archive,
    Folder(i32),
}

impl ListKey {
    pub fn from_list(list: &ChatList) -> Self {
        match list {
            ChatList::Main => ListKey::Main,
            ChatList::Archive => ListKey::Archive,
            ChatList::Folder(f) => ListKey::Folder(f.chat_folder_id),
        }
    }

    pub fn to_list(self) -> ChatList {
        match self {
            ListKey::Main => ChatList::Main,
            ListKey::Archive => ChatList::Archive,
            ListKey::Folder(id) => {
                ChatList::Folder(tdlib_rs::types::ChatListFolder { chat_folder_id: id })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Private(i64),
    BasicGroup,
    Supergroup { id: i64, channel: bool },
    Secret,
}

#[derive(Debug, Clone)]
pub struct ChatInfo {
    pub id: i64,
    pub title: String,
    pub kind: ChatKind,
    pub positions: Vec<ChatPosition>,
    pub unread: i32,
    pub unread_mentions: i32,
    pub marked_unread: bool,
    pub last_read_inbox: i64,
    pub mute_for: i32,
    /// Use the scope's (private, group or channel) mute instead of `mute_for`.
    pub use_default_mute_for: bool,
}

impl ChatInfo {
    pub fn order_in(&self, list: ListKey) -> Option<(bool, i64)> {
        self.positions
            .iter()
            .find(|p| ListKey::from_list(&p.list) == list && p.order != 0)
            .map(|p| (p.is_pinned, p.order))
    }

    /// Which default notification settings apply to this chat.
    pub fn scope(&self) -> NotificationSettingsScope {
        match self.kind {
            ChatKind::Private(_) | ChatKind::Secret => NotificationSettingsScope::PrivateChats,
            ChatKind::BasicGroup | ChatKind::Supergroup { channel: false, .. } => {
                NotificationSettingsScope::GroupChats
            }
            ChatKind::Supergroup { channel: true, .. } => NotificationSettingsScope::ChannelChats,
        }
    }

    pub fn has_unread(&self) -> bool {
        self.unread > 0 || self.marked_unread
    }
}

#[derive(Debug, Clone)]
pub struct Topic {
    pub info: ForumTopicInfo,
    pub unread: i32,
    pub order: i64,
    pub is_pinned: bool,
    pub mute_for: i32,
    /// Use the forum chat's mute instead of `mute_for`.
    pub use_default_mute_for: bool,
}

impl From<ForumTopic> for Topic {
    fn from(t: ForumTopic) -> Self {
        Topic {
            unread: t.unread_count,
            order: t.order,
            is_pinned: t.is_pinned,
            mute_for: t.notification_settings.mute_for,
            use_default_mute_for: t.notification_settings.use_default_mute_for,
            info: t.info,
        }
    }
}

/// `mute_for` of the default notification settings, per scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeMutes {
    pub private: i32,
    pub group: i32,
    pub channel: i32,
}

impl ScopeMutes {
    pub fn get(&self, scope: &NotificationSettingsScope) -> i32 {
        match scope {
            NotificationSettingsScope::PrivateChats => self.private,
            NotificationSettingsScope::GroupChats => self.group,
            NotificationSettingsScope::ChannelChats => self.channel,
        }
    }

    pub fn set(&mut self, scope: &NotificationSettingsScope, mute_for: i32) {
        match scope {
            NotificationSettingsScope::PrivateChats => self.private = mute_for,
            NotificationSettingsScope::GroupChats => self.group = mute_for,
            NotificationSettingsScope::ChannelChats => self.channel = mute_for,
        }
    }
}

/// A hole in a chat's loaded history: messages between `older_end` and the
/// gap's key (its newer end) are not loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub older_end: i64,
    pub loading: bool,
    /// The last load failed; not retried automatically.
    pub failed: bool,
}

impl Gap {
    fn new(older_end: i64) -> Self {
        Self {
            older_end,
            loading: false,
            failed: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryState {
    pub loading: bool,
    pub reached_start: bool,
}

#[derive(Debug, Clone)]
pub struct Typing {
    pub sender: i64,
    pub topic: Option<i32>,
    pub at: Instant,
}

#[derive(Debug, Default)]
pub struct Store {
    pub me: Option<i64>,
    pub chats: HashMap<i64, ChatInfo>,
    pub users: HashMap<i64, User>,
    pub forum_supergroups: HashSet<i64>,
    /// Folder id and name, in Telegram's order.
    pub folders: Vec<(i32, String)>,
    /// Where "All chats" sits among the folders.
    pub main_list_position: usize,
    pub topics: HashMap<i64, Vec<Topic>>,
    pub messages: HashMap<i64, BTreeMap<i64, Message>>,
    pub history: HashMap<View, HistoryState>,
    pub files: HashMap<i32, File>,
    pub typing: HashMap<i64, Vec<Typing>>,
    pub scope_mutes: ScopeMutes,
    /// Per chat: gaps keyed by their newer end.
    pub gaps: HashMap<i64, BTreeMap<i64, Gap>>,
}

pub const TYPING_TTL: Duration = Duration::from_secs(6);

impl Store {
    /// Applies an update. Returns true when anything visible may have changed.
    pub fn apply(&mut self, update: &Update) -> bool {
        match update {
            Update::NewChat(u) => {
                let c = &u.chat;
                let kind = match &c.r#type {
                    ChatType::Private(p) => ChatKind::Private(p.user_id),
                    ChatType::BasicGroup(_) => ChatKind::BasicGroup,
                    ChatType::Supergroup(s) => ChatKind::Supergroup {
                        id: s.supergroup_id,
                        channel: s.is_channel,
                    },
                    ChatType::Secret(_) => ChatKind::Secret,
                };
                self.chats.insert(
                    c.id,
                    ChatInfo {
                        id: c.id,
                        title: c.title.clone(),
                        kind,
                        positions: c.positions.clone(),
                        unread: c.unread_count,
                        unread_mentions: c.unread_mention_count,
                        marked_unread: c.is_marked_as_unread,
                        last_read_inbox: c.last_read_inbox_message_id,
                        mute_for: c.notification_settings.mute_for,
                        use_default_mute_for: c.notification_settings.use_default_mute_for,
                    },
                );
            }
            Update::ChatTitle(u) => {
                if let Some(c) = self.chats.get_mut(&u.chat_id) {
                    c.title = u.title.clone();
                }
            }
            Update::ChatPosition(u) => self.set_position(u.chat_id, &u.position),
            Update::ChatLastMessage(u) => {
                for p in &u.positions {
                    self.set_position(u.chat_id, p);
                }
            }
            Update::ChatDraftMessage(u) => {
                for p in &u.positions {
                    self.set_position(u.chat_id, p);
                }
            }
            Update::ChatReadInbox(u) => {
                if let Some(c) = self.chats.get_mut(&u.chat_id) {
                    c.unread = u.unread_count;
                    c.last_read_inbox = u.last_read_inbox_message_id;
                }
            }
            Update::ChatUnreadMentionCount(u) => {
                if let Some(c) = self.chats.get_mut(&u.chat_id) {
                    c.unread_mentions = u.unread_mention_count;
                }
            }
            Update::ChatIsMarkedAsUnread(u) => {
                if let Some(c) = self.chats.get_mut(&u.chat_id) {
                    c.marked_unread = u.is_marked_as_unread;
                }
            }
            Update::ChatNotificationSettings(u) => {
                if let Some(c) = self.chats.get_mut(&u.chat_id) {
                    c.mute_for = u.notification_settings.mute_for;
                    c.use_default_mute_for = u.notification_settings.use_default_mute_for;
                }
            }
            Update::ScopeNotificationSettings(u) => {
                self.scope_mutes
                    .set(&u.scope, u.notification_settings.mute_for);
            }
            Update::ChatFolders(u) => {
                self.folders = u
                    .chat_folders
                    .iter()
                    .map(|f| (f.id, f.name.text.text.clone()))
                    .collect();
                self.main_list_position =
                    (u.main_chat_list_position.max(0) as usize).min(self.folders.len());
            }
            Update::Supergroup(u) => {
                if u.supergroup.is_forum {
                    self.forum_supergroups.insert(u.supergroup.id);
                } else {
                    self.forum_supergroups.remove(&u.supergroup.id);
                }
            }
            Update::ForumTopicInfo(u) => {
                let topics = self.topics.entry(u.info.chat_id).or_default();
                match topics
                    .iter_mut()
                    .find(|t| t.info.forum_topic_id == u.info.forum_topic_id)
                {
                    Some(t) => t.info = u.info.clone(),
                    None => topics.push(Topic {
                        info: u.info.clone(),
                        unread: 0,
                        order: 0,
                        is_pinned: false,
                        mute_for: 0,
                        use_default_mute_for: true,
                    }),
                }
            }
            Update::ForumTopic(u) => {
                if let Some(t) = self.topics.get_mut(&u.chat_id).and_then(|ts| {
                    ts.iter_mut()
                        .find(|t| t.info.forum_topic_id == u.forum_topic_id)
                }) {
                    t.is_pinned = u.is_pinned;
                    t.mute_for = u.notification_settings.mute_for;
                    t.use_default_mute_for = u.notification_settings.use_default_mute_for;
                }
            }
            Update::User(u) => {
                self.users.insert(u.user.id, u.user.clone());
            }
            Update::NewMessage(u) => {
                self.insert_message(u.message.clone());
            }
            Update::MessageSendSucceeded(u) => {
                if let Some(map) = self.messages.get_mut(&u.message.chat_id) {
                    map.remove(&u.old_message_id);
                }
                self.insert_message(u.message.clone());
            }
            Update::MessageSendFailed(u) => {
                if let Some(map) = self.messages.get_mut(&u.message.chat_id) {
                    map.remove(&u.old_message_id);
                }
                self.insert_message(u.message.clone());
            }
            Update::MessageContent(u) => {
                if let Some(m) = self.message_mut(u.chat_id, u.message_id) {
                    m.content = u.new_content.clone();
                }
            }
            Update::MessageEdited(u) => {
                if let Some(m) = self.message_mut(u.chat_id, u.message_id) {
                    m.edit_date = u.edit_date;
                }
            }
            Update::DeleteMessages(u) => {
                if u.from_cache {
                    return false;
                }
                if let Some(map) = self.messages.get_mut(&u.chat_id) {
                    for id in &u.message_ids {
                        map.remove(id);
                    }
                }
            }
            Update::ChatAction(u) => {
                let MessageSender::User(sender) = &u.sender_id else {
                    return false;
                };
                let topic = topic_of(u.topic_id.as_ref());
                let list = self.typing.entry(u.chat_id).or_default();
                list.retain(|t| t.sender != sender.user_id);
                if !matches!(u.action, ChatAction::Cancel) {
                    list.push(Typing {
                        sender: sender.user_id,
                        topic,
                        at: Instant::now(),
                    });
                }
            }
            Update::File(u) => {
                self.files.insert(u.file.id, u.file.clone());
                return false;
            }
            _ => return false,
        }
        true
    }

    fn set_position(&mut self, chat_id: i64, pos: &ChatPosition) {
        let Some(c) = self.chats.get_mut(&chat_id) else {
            return;
        };
        let key = ListKey::from_list(&pos.list);
        c.positions.retain(|p| ListKey::from_list(&p.list) != key);
        if pos.order != 0 {
            c.positions.push(pos.clone());
        }
    }

    pub fn insert_message(&mut self, m: Message) {
        self.messages.entry(m.chat_id).or_default().insert(m.id, m);
    }

    /// Whether notifications for the chat are off, following Telegram's
    /// default for its kind of chat when the chat uses the default.
    pub fn chat_muted(&self, c: &ChatInfo) -> bool {
        let mute_for = if c.use_default_mute_for {
            self.scope_mutes.get(&c.scope())
        } else {
            c.mute_for
        };
        mute_for > 0
    }

    /// A topic on default settings follows its forum chat.
    pub fn topic_muted(&self, c: &ChatInfo, t: &Topic) -> bool {
        if t.use_default_mute_for {
            self.chat_muted(c)
        } else {
            t.mute_for > 0
        }
    }

    /// Whether a new message should stay silent: its topic's or chat's mute.
    pub fn message_muted(&self, m: &Message) -> bool {
        let Some(c) = self.chats.get(&m.chat_id) else {
            return false;
        };
        let topic = topic_of(m.topic_id.as_ref()).and_then(|id| {
            self.topics
                .get(&m.chat_id)?
                .iter()
                .find(|t| t.info.forum_topic_id == id)
        });
        match topic {
            Some(t) => self.topic_muted(c, t),
            None => self.chat_muted(c),
        }
    }

    /// Inserts a contiguous page of history (for example the messages around
    /// a reply target) and keeps track of the holes it leaves or fills.
    /// `newer_end` is a loaded id the page is known to reach up to (the
    /// `from` of a backwards page), if any.
    pub fn insert_page(&mut self, chat_id: i64, msgs: Vec<Message>, newer_end: Option<i64>) {
        let map = self.messages.entry(chat_id).or_default();
        // The page only joins `newer_end` if that message is still loaded; a
        // trim while the page was on its way leaves it detached.
        let newer_end = newer_end.filter(|n| map.contains_key(n));
        let lo = msgs.iter().map(|m| m.id).min();
        let hi = msgs.iter().map(|m| m.id).max().max(newer_end);
        let (Some(lo), Some(hi)) = (lo.or(newer_end), hi) else {
            return;
        };
        let overlaps = newer_end.is_some() || map.range(lo..=hi).next().is_some();
        let older = map.range(..lo).next_back().map(|(id, _)| *id);
        let newer = map.range(hi + 1..).next().map(|(id, _)| *id);
        for m in msgs {
            map.insert(m.id, m);
        }
        let gaps = self.gaps.entry(chat_id).or_default();
        // The page fills whatever part of a known gap it covers.
        let hit: Vec<(i64, Gap)> = gaps
            .iter()
            .filter(|(b, g)| g.older_end < hi && lo < **b)
            .map(|(b, g)| (*b, *g))
            .collect();
        for (b, g) in hit {
            gaps.remove(&b);
            if g.older_end < lo {
                gaps.insert(lo, Gap::new(g.older_end));
            }
            if hi < b {
                gaps.insert(b, Gap::new(hi));
            }
        }
        // A page that touches nothing loaded is cut off from its neighbours.
        if !overlaps {
            if let Some(n) = newer {
                gaps.entry(n).or_insert(Gap::new(hi));
            }
            if let Some(o) = older {
                gaps.entry(lo).or_insert(Gap::new(o));
            }
        }
        if gaps.is_empty() {
            self.gaps.remove(&chat_id);
        }
    }

    /// Gaps of a chat, as (older end, newer end).
    #[cfg(test)]
    pub fn gaps_of(&self, chat_id: i64) -> Vec<(i64, i64)> {
        self.gaps
            .get(&chat_id)
            .map(|g| g.iter().map(|(b, g)| (g.older_end, *b)).collect())
            .unwrap_or_default()
    }

    /// Lets failed gap loads run again: for one chat, or all with `None`.
    pub fn retry_failed_gaps(&mut self, chat_id: Option<i64>) {
        for (c, gaps) in self.gaps.iter_mut() {
            if chat_id.is_none_or(|id| id == *c) {
                for g in gaps.values_mut() {
                    g.failed = false;
                }
            }
        }
    }

    pub fn gap_mut(&mut self, chat_id: i64, newer_end: i64) -> Option<&mut Gap> {
        self.gaps.get_mut(&chat_id)?.get_mut(&newer_end)
    }

    /// A backwards page loaded from a gap's newer end arrived. An empty page
    /// means there is nothing in between.
    pub fn fill_gap(&mut self, chat_id: i64, newer_end: i64, msgs: Vec<Message>) {
        if msgs.is_empty() {
            if let Some(g) = self.gaps.get_mut(&chat_id) {
                g.remove(&newer_end);
            }
            return;
        }
        self.insert_page(chat_id, msgs, Some(newer_end));
    }

    pub fn message(&self, chat_id: i64, id: i64) -> Option<&Message> {
        self.messages.get(&chat_id)?.get(&id)
    }

    fn message_mut(&mut self, chat_id: i64, id: i64) -> Option<&mut Message> {
        self.messages.get_mut(&chat_id)?.get_mut(&id)
    }

    /// Chats of a list in Telegram's order: by the pair (`order`, chat id),
    /// descending. Pinned chats have the highest orders.
    pub fn chats_in(&self, list: ListKey) -> Vec<&ChatInfo> {
        let mut v: Vec<(&ChatInfo, i64)> = self
            .chats
            .values()
            .filter_map(|c| c.order_in(list).map(|(_, o)| (c, o)))
            .collect();
        v.sort_by_key(|(c, o)| std::cmp::Reverse((*o, c.id)));
        v.into_iter().map(|(c, _)| c).collect()
    }

    /// Keeps only the newest `keep` messages of a chat. The views of that
    /// chat can then page back again, and gaps below the cut are forgotten.
    pub fn trim_chat(&mut self, chat_id: i64, keep: usize) {
        let Some(map) = self.messages.get_mut(&chat_id) else {
            return;
        };
        if keep == 0 || map.len() <= keep {
            return;
        }
        let Some(&cut) = map.keys().nth(map.len() - keep) else {
            return;
        };
        *map = map.split_off(&cut);
        for (v, st) in self.history.iter_mut() {
            if v.chat_id == chat_id {
                st.reached_start = false;
            }
        }
        if let Some(g) = self.gaps.get_mut(&chat_id) {
            g.retain(|_, gap| gap.older_end >= cut);
        }
    }

    /// Forgets file records that no loaded message refers to. Returns the
    /// ids still in use.
    pub fn evict_files(&mut self) -> HashSet<i32> {
        let live: HashSet<i32> = self
            .messages
            .values()
            .flat_map(|m| m.values())
            .flat_map(message_file_ids)
            .collect();
        self.files.retain(|id, _| live.contains(id));
        live
    }

    pub fn is_forum(&self, chat_id: i64) -> bool {
        matches!(
            self.chats.get(&chat_id).map(|c| c.kind),
            Some(ChatKind::Supergroup { id, .. }) if self.forum_supergroups.contains(&id)
        )
    }

    /// Topics sorted pinned first, then most recent.
    pub fn topics_of(&self, chat_id: i64) -> Vec<&Topic> {
        let mut v: Vec<&Topic> = self
            .topics
            .get(&chat_id)
            .map(|t| t.iter().collect())
            .unwrap_or_default();
        v.retain(|t| !t.info.is_hidden || t.info.is_general);
        // TDLib: sort by `order` alone; pinned topics already come first.
        v.sort_by_key(|t| std::cmp::Reverse(t.order));
        v
    }

    pub fn set_topics(&mut self, chat_id: i64, topics: Vec<ForumTopic>) {
        self.topics
            .insert(chat_id, topics.into_iter().map(Topic::from).collect());
    }

    pub fn topic_name(&self, chat_id: i64, topic: i32) -> Option<&str> {
        self.topics
            .get(&chat_id)?
            .iter()
            .find(|t| t.info.forum_topic_id == topic)
            .map(|t| t.info.name.as_str())
    }

    pub fn user_name(&self, user_id: i64) -> String {
        match self.users.get(&user_id) {
            Some(u) => {
                let name = format!("{} {}", u.first_name, u.last_name)
                    .trim()
                    .to_string();
                if name.is_empty() {
                    "Deleted Account".into()
                } else {
                    name
                }
            }
            None => format!("user {user_id}"),
        }
    }

    pub fn sender_name(&self, sender: &MessageSender) -> String {
        match sender {
            MessageSender::User(u) => self.user_name(u.user_id),
            MessageSender::Chat(c) => self
                .chats
                .get(&c.chat_id)
                .map(|c| c.title.clone())
                .unwrap_or_else(|| format!("chat {}", c.chat_id)),
        }
    }

    pub fn sender_id(sender: &MessageSender) -> i64 {
        match sender {
            MessageSender::User(u) => u.user_id,
            MessageSender::Chat(c) => c.chat_id,
        }
    }

    pub fn chat_title(&self, chat_id: i64) -> String {
        self.chats
            .get(&chat_id)
            .map(|c| c.title.clone())
            .unwrap_or_default()
    }

    /// Messages of a view, oldest first.
    pub fn view_messages(&self, view: View) -> Vec<&Message> {
        let Some(map) = self.messages.get(&view.chat_id) else {
            return vec![];
        };
        map.values()
            .filter(|m| match view.topic {
                None => true,
                Some(t) => message_in_topic(m, t),
            })
            .collect()
    }

    pub fn oldest_id(&self, view: View) -> Option<i64> {
        self.view_messages(view).first().map(|m| m.id)
    }

    pub fn typing_in(&self, view: View) -> Vec<String> {
        let now = Instant::now();
        self.typing
            .get(&view.chat_id)
            .map(|v| {
                v.iter()
                    .filter(|t| now.duration_since(t.at) < TYPING_TTL)
                    .filter(|t| view.topic.is_none() || t.topic == view.topic)
                    .filter(|t| Some(t.sender) != self.me)
                    .map(|t| self.user_name(t.sender))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn expire_typing(&mut self) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for v in self.typing.values_mut() {
            let before = v.len();
            v.retain(|t| now.duration_since(t.at) < TYPING_TTL);
            changed |= v.len() != before;
        }
        changed
    }
}

/// Files a message shows or opens.
fn message_file_ids(m: &Message) -> Vec<i32> {
    use tdlib_rs::enums::MessageContent as C;
    match &m.content {
        C::MessagePhoto(p) => p.photo.sizes.iter().map(|s| s.photo.id).collect(),
        C::MessageDocument(d) => vec![d.document.document.id],
        C::MessageVideo(v) => vec![v.video.video.id],
        C::MessageAnimation(a) => vec![a.animation.animation.id],
        C::MessageAudio(a) => vec![a.audio.audio.id],
        C::MessageVoiceNote(v) => vec![v.voice_note.voice.id],
        C::MessageVideoNote(v) => vec![v.video_note.video.id],
        C::MessageSticker(s) => vec![s.sticker.sticker.id],
        _ => vec![],
    }
}

pub fn topic_of(t: Option<&MessageTopic>) -> Option<i32> {
    match t {
        Some(MessageTopic::Forum(f)) => Some(f.forum_topic_id),
        _ => None,
    }
}

pub fn message_in_topic(m: &Message, topic: i32) -> bool {
    match topic_of(m.topic_id.as_ref()) {
        Some(t) => t == topic,
        // Messages without a topic belong to "General" (id 1).
        None => topic == 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    fn chat(store: &mut Store, id: i64, kind: ChatKind, mute_for: i32, default: bool) {
        store.chats.insert(
            id,
            ChatInfo {
                id,
                title: format!("chat {id}"),
                kind,
                positions: vec![],
                unread: 0,
                unread_mentions: 0,
                marked_unread: false,
                last_read_inbox: 0,
                mute_for,
                use_default_mute_for: default,
            },
        );
    }

    fn scope_update(scope: NotificationSettingsScope, mute_for: i32) -> Update {
        serde_json::from_value(serde_json::json!({
            "@type": "updateScopeNotificationSettings",
            "scope": serde_json::to_value(&scope).unwrap(),
            "notification_settings": {
                "mute_for": mute_for, "sound_id": "0", "show_preview": true,
                "use_default_mute_stories": true, "mute_stories": false, "story_sound_id": "0",
                "show_story_poster": true, "disable_pinned_message_notifications": false,
                "disable_mention_notifications": false
            }
        }))
        .expect("scope update json")
    }

    const CHANNEL: ChatKind = ChatKind::Supergroup {
        id: 7,
        channel: true,
    };
    const GROUP: ChatKind = ChatKind::Supergroup {
        id: 8,
        channel: false,
    };

    #[test]
    fn default_mute_follows_the_scope() {
        let mut store = Store::default();
        chat(&mut store, 1, CHANNEL, 0, true);
        chat(&mut store, 2, GROUP, 0, true);
        chat(&mut store, 3, CHANNEL, 0, false);
        chat(&mut store, 4, ChatKind::Private(9), 3600, false);
        let muted = |s: &Store, id| s.chat_muted(&s.chats[&id]);
        assert!(!muted(&store, 1), "scope not muted yet");

        store.apply(&scope_update(
            NotificationSettingsScope::ChannelChats,
            86400,
        ));
        assert!(muted(&store, 1), "default channel follows the muted scope");
        assert!(!muted(&store, 2), "groups have their own scope");
        assert!(!muted(&store, 3), "explicit unmute wins over the scope");
        assert!(muted(&store, 4), "explicit mute");

        store.apply(&scope_update(NotificationSettingsScope::ChannelChats, 0));
        assert!(!muted(&store, 1));
    }

    #[test]
    fn topic_on_default_inherits_its_chat() {
        let mut store = Store::default();
        chat(&mut store, 1, GROUP, 0, true);
        let topic = |mute_for, default| {
            Topic {
            info: serde_json::from_value(serde_json::json!({
                "chat_id": 1, "forum_topic_id": 5, "name": "t", "icon": {"color": 0, "custom_emoji_id": "0"},
                "creation_date": 0, "creator_id": {"@type": "messageSenderUser", "user_id": 1},
                "is_general": false, "is_outgoing": false, "is_closed": false, "is_hidden": false,
                "is_name_implicit": false
            }))
            .expect("topic info json"),
            unread: 0,
            order: 0,
            is_pinned: false,
            mute_for,
            use_default_mute_for: default,
        }
        };
        let c = store.chats[&1].clone();
        assert!(!store.topic_muted(&c, &topic(0, true)));
        store.scope_mutes.group = 3600;
        assert!(
            store.topic_muted(&c, &topic(0, true)),
            "chat is muted by scope"
        );
        assert!(
            !store.topic_muted(&c, &topic(0, false)),
            "topic unmuted itself"
        );
        store.scope_mutes.group = 0;
        assert!(store.topic_muted(&c, &topic(60, false)));
    }

    fn page(ids: std::ops::RangeInclusive<i64>) -> Vec<Message> {
        ids.map(|id| {
            testutil::message(
                id,
                10,
                2,
                testutil::BASE_DATE + id as i32,
                testutil::text_content("hello"),
                None,
            )
        })
        .collect()
    }

    #[test]
    fn a_detached_page_leaves_a_gap_that_backward_pages_close() {
        let mut store = Store::default();
        store.insert_page(10, page(100..=110), None);
        assert!(store.gaps_of(10).is_empty());
        store.insert_page(10, page(40..=50), None);
        assert_eq!(store.gaps_of(10), vec![(50, 100)]);
        // Jumping into the same hole again splits it.
        store.insert_page(10, page(70..=72), None);
        assert_eq!(store.gaps_of(10), vec![(50, 70), (72, 100)]);
        // A backwards page from 100 that stops short shrinks the gap.
        store.fill_gap(10, 100, page(80..=99));
        assert_eq!(store.gaps_of(10), vec![(50, 70), (72, 80)]);
        store.fill_gap(10, 80, page(60..=79));
        assert_eq!(store.gaps_of(10), vec![(50, 60)]);
        store.fill_gap(10, 60, vec![]);
        assert!(store.gaps_of(10).is_empty());
    }

    #[test]
    fn a_page_from_a_trimmed_message_is_detached() {
        let mut store = Store::default();
        store.insert_page(10, page(100..=110), None);
        // Loaded backwards from 95, but 95 was trimmed meanwhile.
        store.insert_page(10, page(80..=89), Some(95));
        assert_eq!(store.gaps_of(10), vec![(89, 100)]);
    }

    #[test]
    fn failed_gaps_can_be_retried() {
        let mut store = Store::default();
        store.insert_page(10, page(100..=110), None);
        store.insert_page(10, page(40..=50), None);
        store.gap_mut(10, 100).unwrap().failed = true;
        store.retry_failed_gaps(Some(11));
        assert!(store.gap_mut(10, 100).unwrap().failed, "other chat");
        store.retry_failed_gaps(Some(10));
        assert!(!store.gap_mut(10, 100).unwrap().failed);
    }

    #[test]
    fn trimming_keeps_the_newest_and_lets_the_view_page_back() {
        let mut store = Store::default();
        store.insert_page(10, page(1..=600), None);
        let view = View {
            chat_id: 10,
            topic: None,
        };
        store.history.entry(view).or_default().reached_start = true;
        store.trim_chat(10, 500);
        let ids: Vec<i64> = store.messages[&10].keys().copied().collect();
        assert_eq!(ids.len(), 500);
        assert_eq!((ids[0], ids[499]), (101, 600));
        assert!(!store.history[&view].reached_start);
    }

    #[test]
    fn files_without_a_message_are_forgotten() {
        let mut store = Store::default();
        store.insert_message(testutil::message(
            1,
            10,
            2,
            testutil::BASE_DATE,
            testutil::photo_content(77, "/x.jpg", 10, 10),
            None,
        ));
        store.files.insert(77, testutil::file(77, "/x.jpg", true));
        store.files.insert(78, testutil::file(78, "/y.jpg", true));
        store.evict_files();
        assert!(store.files.contains_key(&77) && !store.files.contains_key(&78));
    }

    #[test]
    fn equal_orders_break_ties_by_chat_id() {
        let mut store = Store::default();
        for id in [3, 1, 2] {
            chat(&mut store, id, GROUP, 0, false);
            store.chats.get_mut(&id).unwrap().positions = vec![ChatPosition {
                list: ListKey::Main.to_list(),
                order: 5,
                is_pinned: false,
                source: None,
            }];
        }
        let ids: Vec<i64> = store.chats_in(ListKey::Main).iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }
}
