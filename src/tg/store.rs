//! In-memory model of chats, folders, users and messages, kept current from
//! TDLib updates.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use tdlib_rs::enums::{ChatAction, ChatList, ChatType, MessageSender, MessageTopic, Update};
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
}

impl ChatInfo {
    pub fn order_in(&self, list: ListKey) -> Option<(bool, i64)> {
        self.positions
            .iter()
            .find(|p| ListKey::from_list(&p.list) == list && p.order != 0)
            .map(|p| (p.is_pinned, p.order))
    }

    pub fn muted(&self) -> bool {
        self.mute_for > 0
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
}

impl From<ForumTopic> for Topic {
    fn from(t: ForumTopic) -> Self {
        Topic {
            unread: t.unread_count,
            order: t.order,
            is_pinned: false,
            mute_for: t.notification_settings.mute_for,
            info: t.info,
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
                }
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

    pub fn message(&self, chat_id: i64, id: i64) -> Option<&Message> {
        self.messages.get(&chat_id)?.get(&id)
    }

    fn message_mut(&mut self, chat_id: i64, id: i64) -> Option<&mut Message> {
        self.messages.get_mut(&chat_id)?.get_mut(&id)
    }

    /// Chats of a list, Telegram order: pinned first, then by `order` descending.
    pub fn chats_in(&self, list: ListKey) -> Vec<&ChatInfo> {
        let mut v: Vec<(&ChatInfo, (bool, i64))> = self
            .chats
            .values()
            .filter_map(|c| c.order_in(list).map(|o| (c, o)))
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v.into_iter().map(|(c, _)| c).collect()
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
        v.sort_by_key(|t| std::cmp::Reverse((t.is_pinned, t.order)));
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
