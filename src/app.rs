//! Application state, event loop and action handling.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEvent, KeyEventKind, MouseButton, MouseEventKind};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui_image::protocol::Protocol;
use ratatui_textarea::TextArea;
use tdlib_rs::enums::{self, AuthorizationState, MessageContent, Update};
use tdlib_rs::functions;
use tdlib_rs::types::{File, ForumTopic, Message};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::Config;
use crate::keymap::{
    ComposerAction, GlobalAction, Keymap, MessagesAction, PickerAction, TreeAction,
};
use crate::tg::store::{ListKey, Store};
use crate::tg::{Params, Tg, View};
use crate::ui::chats_tree::{self, NodeId, TreeState};
use crate::ui::composer::{Composer, Mode};
use crate::ui::image::{self, Images, Wanted};
use crate::ui::messages::{self, MessagesState};
use crate::ui::picker::ChatPicker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Messages,
    Composer,
}

pub enum Modal {
    None,
    Picker(ChatPicker),
    Help,
    ConfirmDelete { chat_id: i64, message_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    Starting,
    Phone,
    Qr(String),
    Code(String),
    Password(String),
    Email,
    EmailCode,
    Registration,
    Ready,
    Closing,
}

#[derive(Debug, Clone)]
pub enum FilePurpose {
    Image(Wanted),
    Open,
    Save(PathBuf),
}

pub enum AppEvent {
    History {
        view: View,
        from: i64,
        result: Result<Vec<Message>, String>,
    },
    Around {
        view: View,
        target: i64,
        result: Result<Vec<Message>, String>,
    },
    Topics {
        chat_id: i64,
        result: Result<Vec<ForumTopic>, String>,
    },
    File {
        purpose: FilePurpose,
        result: Result<File, String>,
    },
    ImageReady {
        file_id: i32,
        result: Result<Protocol, String>,
    },
    Status(String),
    AuthError(String),
    Me(i64),
}

/// Something the loop must do outside of normal event handling.
pub enum Interrupt {
    Editor,
    Suspend,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Rects {
    pub tree: Rect,
    pub messages: Rect,
    pub composer: Rect,
    /// Inner message area and its first visible layout line.
    pub messages_inner: Rect,
    pub messages_top: usize,
    /// Blank rows above the first line when the history is shorter than the pane.
    pub messages_pad: u16,
    pub tree_inner: Rect,
}

pub struct App {
    pub cfg: Config,
    pub keys: Keymap,
    pub tg: Tg,
    pub params: Params,
    pub store: Store,
    pub auth: Auth,
    pub login_input: TextArea<'static>,
    pub login_error: Option<String>,
    pub login_busy: bool,
    pub focus: Focus,
    pub sidebar: bool,
    pub tree: TreeState,
    pub msgs: MessagesState,
    pub composer: Composer,
    pub modal: Modal,
    pub current: Option<View>,
    pub images: Images,
    pub status: Option<(String, Instant)>,
    pub terminal_focused: bool,
    pub rects: Rects,
    /// Message id ranges from the last draw, for mouse clicks.
    pub last_ranges: Vec<(i64, usize, usize)>,
    /// Images the last draw wanted.
    pub wanted: Vec<Wanted>,
    pub tx: UnboundedSender<AppEvent>,
    pub quit_at: Option<Instant>,
    pub logging_out: bool,
    pub dirty: bool,
    /// Connection banner, None when connected.
    pub connection: Option<&'static str>,
    tg_closed: bool,
    pending_interrupt: Option<Interrupt>,
}

fn login_area(mask: bool) -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_cursor_line_style(ratatui::style::Style::default());
    if mask {
        ta.set_mask_char('•');
    }
    ta
}

impl App {
    pub fn new(
        cfg: Config,
        keys: Keymap,
        tg: Tg,
        params: Params,
        images: Images,
        tx: UnboundedSender<AppEvent>,
    ) -> Self {
        let sidebar = true;
        Self {
            cfg,
            keys,
            tg,
            params,
            store: Store::default(),
            auth: Auth::Starting,
            login_input: login_area(false),
            login_error: None,
            login_busy: false,
            focus: Focus::Tree,
            sidebar,
            tree: TreeState::default(),
            msgs: MessagesState::default(),
            composer: Composer::default(),
            modal: Modal::None,
            current: None,
            images,
            status: None,
            terminal_focused: true,
            rects: Rects::default(),
            last_ranges: Vec::new(),
            wanted: Vec::new(),
            tx,
            quit_at: None,
            logging_out: false,
            dirty: true,
            connection: None,
            tg_closed: false,
            pending_interrupt: None,
        }
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
        self.dirty = true;
    }

    /// Runs until quit. Returns an interrupt when the terminal must be handed
    /// to another program (editor, shell suspend); call again afterwards.
    pub async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        td_rx: &mut UnboundedReceiver<Update>,
        app_rx: &mut UnboundedReceiver<AppEvent>,
    ) -> Result<Option<Interrupt>> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            if self.dirty {
                self.dirty = false;
                terminal.draw(|f| crate::ui::draw(f, self))?;
                self.after_draw();
            }
            if let Some(at) = self.quit_at
                && Instant::now() >= at
            {
                return Ok(None);
            }
            tokio::select! {
                ev = events.next() => match ev {
                    Some(Ok(ev)) => self.on_terminal(ev),
                    Some(Err(e)) => tracing::warn!("terminal event: {e}"),
                    None => return Ok(None),
                },
                Some(update) = td_rx.recv() => {
                    self.on_update(update);
                    // Drain bursts (startup sends thousands) before redrawing.
                    while let Ok(u) = td_rx.try_recv() {
                        self.on_update(u);
                    }
                }
                Some(ev) = app_rx.recv() => self.on_app_event(ev),
                _ = tick.tick() => {
                    if self.store.expire_typing() {
                        self.dirty = true;
                    }
                    if let Some((_, at)) = &self.status && at.elapsed() > Duration::from_secs(5) {
                        self.status = None;
                        self.dirty = true;
                    }
                }
            }
            if self.quit_at.is_some() && matches!(self.auth, Auth::Closing) && self.tg_closed {
                return Ok(None);
            }
            if let Some(i) = self.pending_interrupt.take() {
                return Ok(Some(i));
            }
        }
    }

    // ---------------------------------------------------------------- TDLib

    fn on_update(&mut self, update: Update) {
        match &update {
            Update::AuthorizationState(s) => {
                self.on_auth(s.authorization_state.clone());
                self.dirty = true;
                return;
            }
            Update::NewMessage(u) => self.maybe_notify(&u.message),
            Update::ConnectionState(s) => {
                let label = match s.state {
                    enums::ConnectionState::WaitingForNetwork => Some("waiting for network…"),
                    enums::ConnectionState::ConnectingToProxy => Some("connecting to proxy…"),
                    enums::ConnectionState::Connecting => Some("connecting…"),
                    enums::ConnectionState::Updating => Some("updating…"),
                    enums::ConnectionState::Ready => None,
                };
                self.connection = label;
                self.dirty = true;
            }
            Update::ChatFolders(u) => {
                for f in &u.chat_folders {
                    self.spawn_load_chats(ListKey::Folder(f.id));
                }
            }
            _ => {}
        }
        let current_new = matches!(&update, Update::NewMessage(u) if self.current.is_some_and(|v| v.chat_id == u.message.chat_id));
        if self.store.apply(&update) {
            self.dirty = true;
        }
        if current_new && let Update::NewMessage(u) = &update {
            self.mark_read(u.message.chat_id, vec![u.message.id]);
        }
    }

    fn on_auth(&mut self, state: AuthorizationState) {
        tracing::info!("auth state: {state:?}");
        self.login_busy = false;
        let prev_qr = matches!(self.auth, Auth::Qr(_));
        self.auth = match state {
            AuthorizationState::WaitTdlibParameters => {
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                let p = self.params.clone();
                tokio::spawn(async move {
                    if let Err(e) = tg.set_parameters(&p).await {
                        let _ = tx.send(AppEvent::AuthError(format!("{e:#}")));
                    }
                });
                Auth::Starting
            }
            AuthorizationState::WaitPhoneNumber => {
                if prev_qr {
                    self.login_error = Some("QR login cancelled or expired".into());
                }
                self.login_input = login_area(false);
                Auth::Phone
            }
            AuthorizationState::WaitOtherDeviceConfirmation(c) => Auth::Qr(c.link),
            AuthorizationState::WaitCode(c) => {
                self.login_input = login_area(false);
                let how = match c.code_info.r#type {
                    enums::AuthenticationCodeType::TelegramMessage(_) => {
                        "sent to your Telegram app"
                    }
                    enums::AuthenticationCodeType::Sms(_) => "sent by SMS",
                    enums::AuthenticationCodeType::Call(_) => "sent by phone call",
                    _ => "sent",
                };
                Auth::Code(format!("Code {how} to {}", c.code_info.phone_number))
            }
            AuthorizationState::WaitPassword(p) => {
                self.login_input = login_area(true);
                Auth::Password(p.password_hint)
            }
            AuthorizationState::WaitEmailAddress(_) => {
                self.login_input = login_area(false);
                Auth::Email
            }
            AuthorizationState::WaitEmailCode(_) => {
                self.login_input = login_area(false);
                Auth::EmailCode
            }
            AuthorizationState::WaitRegistration(_) => Auth::Registration,
            AuthorizationState::WaitPremiumPurchase(_) => {
                self.login_error =
                    Some("Telegram requires Premium to log in this account here.".into());
                Auth::Registration
            }
            AuthorizationState::Ready => {
                self.login_error = None;
                self.on_ready();
                Auth::Ready
            }
            AuthorizationState::LoggingOut | AuthorizationState::Closing => Auth::Closing,
            AuthorizationState::Closed => {
                self.tg_closed = true;
                if self.logging_out {
                    crate::tg::forget_database_key(&self.params.key_store);
                    if let Err(e) = std::fs::remove_dir_all(&self.params.database_dir) {
                        tracing::warn!("removing session: {e}");
                    }
                }
                if self.quit_at.is_none() {
                    self.quit_at = Some(Instant::now());
                }
                Auth::Closing
            }
        };
    }

    fn on_ready(&mut self) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match functions::get_me(tg.client_id).await {
                Ok(enums::User::User(u)) => {
                    let _ = tx.send(AppEvent::Status(format!(
                        "logged in as {} {}",
                        u.first_name, u.last_name
                    )));
                }
                Err(e) => tracing::warn!("get_me: {}", e.message),
            }
        });
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(enums::OptionValue::Integer(i)) =
                functions::get_option("my_id".into(), tg.client_id).await
            {
                let _ = tx.send(AppEvent::Me(i.value));
            }
        });
        self.spawn_load_chats(ListKey::Main);
        self.spawn_load_chats(ListKey::Archive);
        self.focus = Focus::Tree;
    }

    fn spawn_load_chats(&self, list: ListKey) {
        let tg = self.tg.clone();
        tokio::spawn(async move { tg.load_all_chats(list.to_list()).await });
    }

    pub(crate) fn on_app_event(&mut self, ev: AppEvent) {
        self.dirty = true;
        match ev {
            AppEvent::History { view, from, result } => {
                let st = self.store.history.entry(view).or_default();
                st.loading = false;
                match result {
                    Ok(msgs) => {
                        // Fewer than asked is normal (TDLib returns what it has locally first);
                        // only an empty page means the start of the chat.
                        let got_older = msgs.iter().any(|m| from == 0 || m.id < from);
                        if !got_older {
                            st.reached_start = true;
                        }
                        for m in msgs {
                            self.store.insert_message(m);
                        }
                        if Some(view) == self.current && from == 0 {
                            let ids: Vec<i64> = self
                                .store
                                .view_messages(view)
                                .iter()
                                .rev()
                                .take(10)
                                .map(|m| m.id)
                                .collect();
                            self.mark_read(view.chat_id, ids);
                        }
                    }
                    Err(e) => self.set_status(format!("history: {e}")),
                }
            }
            AppEvent::Around {
                view,
                target,
                result,
            } => match result {
                Ok(msgs) => {
                    for m in msgs {
                        self.store.insert_message(m);
                    }
                    if Some(view) == self.current
                        && self.store.message(view.chat_id, target).is_some()
                    {
                        self.msgs.selected = Some(target);
                        self.msgs.reveal = true;
                    }
                }
                Err(e) => self.set_status(format!("jump: {e}")),
            },
            AppEvent::Topics { chat_id, result } => match result {
                Ok(t) => self.store.set_topics(chat_id, t),
                Err(e) => self.set_status(format!("topics: {e}")),
            },
            AppEvent::File { purpose, result } => match (purpose, result) {
                (FilePurpose::Image(w), Ok(f)) if f.local.is_downloading_completed => {
                    self.spawn_encode(w, f.local.path)
                }
                (FilePurpose::Image(w), Ok(_)) | (FilePurpose::Image(w), Err(_)) => {
                    self.images.entries.insert(w.file_id, image::Entry::Failed);
                }
                (FilePurpose::Open, Ok(f)) => open_path(&f.local.path),
                (FilePurpose::Save(dest), Ok(f)) => {
                    match std::fs::copy(&f.local.path, unique_path(&dest)) {
                        Ok(_) => self.set_status(format!(
                            "saved to {}",
                            dest.parent().unwrap_or(&dest).display()
                        )),
                        Err(e) => self.set_status(format!("save failed: {e}")),
                    }
                }
                (_, Err(e)) => self.set_status(format!("download failed: {e}")),
            },
            AppEvent::ImageReady { file_id, result } => {
                let entry = match result {
                    Ok(p) => image::Entry::Ready(p),
                    Err(e) => {
                        tracing::warn!("image {file_id}: {e}");
                        image::Entry::Failed
                    }
                };
                self.images.entries.insert(file_id, entry);
            }
            AppEvent::Status(s) => {
                if !s.is_empty() {
                    self.set_status(s);
                }
            }
            AppEvent::Me(id) => self.store.me = Some(id),
            AppEvent::AuthError(e) => {
                self.login_busy = false;
                self.login_error = Some(e);
            }
        }
    }

    /// Work that depends on what the last frame showed.
    pub(crate) fn after_draw(&mut self) {
        if let Some(view) = self.current {
            let st = self.store.history.get(&view).copied().unwrap_or_default();
            if self.msgs.at_top && !st.loading && !st.reached_start {
                self.load_history(view);
            }
        }
        let wanted = std::mem::take(&mut self.wanted);
        for w in wanted {
            if self.images.entries.contains_key(&w.file_id) {
                continue;
            }
            self.images
                .entries
                .insert(w.file_id, image::Entry::Requested);
            match self.store.files.get(&w.file_id) {
                Some(f) if f.local.is_downloading_completed => {
                    let path = f.local.path.clone();
                    self.spawn_encode(w, path);
                }
                _ => self.spawn_download(w.file_id, FilePurpose::Image(w), 8),
            }
        }
    }

    fn spawn_encode(&self, w: Wanted, path: String) {
        let Some(picker) = self.images.picker.clone() else {
            return;
        };
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result =
                image::encode(&picker, &path, w.cols, w.rows).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::ImageReady {
                file_id: w.file_id,
                result,
            });
        });
    }

    fn spawn_download(&self, file_id: i32, purpose: FilePurpose, priority: i32) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .download(file_id, priority, true)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::File { purpose, result });
        });
    }

    fn load_history(&mut self, view: View) {
        let st = self.store.history.entry(view).or_default();
        if st.loading {
            return;
        }
        st.loading = true;
        let from = self.store.oldest_id(view).unwrap_or(0);
        let limit = self.cfg.messages_limit;
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg
                .history(view, from, limit)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::History { view, from, result });
        });
    }

    fn mark_read(&self, chat_id: i64, ids: Vec<i64>) {
        if ids.is_empty() || !self.terminal_focused {
            return;
        }
        let tg = self.tg.clone();
        tokio::spawn(async move {
            if let Err(e) = functions::view_messages(chat_id, ids, None, true, tg.client_id).await {
                tracing::debug!("view_messages: {}", e.message);
            }
        });
    }

    fn maybe_notify(&self, m: &Message) {
        let n = &self.cfg.notifications;
        if !n.enabled || m.is_outgoing || !matches!(self.auth, Auth::Ready) {
            return;
        }
        let Some(chat) = self.store.chats.get(&m.chat_id) else {
            return;
        };
        if chat.muted() {
            return;
        }
        let viewing = self.current.is_some_and(|v| v.chat_id == m.chat_id);
        if n.when_unfocused && self.terminal_focused {
            return;
        }
        if !n.when_unfocused && viewing && self.terminal_focused {
            return;
        }
        let title = chat.title.clone();
        let sender = self.store.sender_name(&m.sender_id);
        let body = messages::snippet(m, &self.store);
        let body = if sender == title {
            body
        } else {
            format!("{sender}: {body}")
        };
        crate::notify::send(title, body);
    }

    // ---------------------------------------------------------------- terminal

    fn on_terminal(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                self.dirty = true;
                self.on_key(k);
            }
            Event::Mouse(m) if self.cfg.mouse => self.on_mouse(m),
            Event::Paste(s) => {
                if self.focus == Focus::Composer && matches!(self.auth, Auth::Ready) {
                    self.composer.ta.insert_str(&s);
                } else if !matches!(self.auth, Auth::Ready) {
                    self.login_input.insert_str(s.trim());
                }
                self.dirty = true;
            }
            Event::FocusGained => {
                self.terminal_focused = true;
                if let Some(v) = self.current {
                    let ids: Vec<i64> = self
                        .store
                        .view_messages(v)
                        .iter()
                        .rev()
                        .take(10)
                        .map(|m| m.id)
                        .collect();
                    self.mark_read(v.chat_id, ids);
                }
            }
            Event::FocusLost => self.terminal_focused = false,
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        // Quit works everywhere, even mid-login.
        if self.keys.global.lookup(&k) == Some(GlobalAction::Quit) {
            self.quit();
            return;
        }
        if !matches!(self.auth, Auth::Ready) {
            self.on_login_key(k);
            return;
        }
        match &mut self.modal {
            Modal::Picker(_) => return self.on_picker_key(k),
            Modal::Help => {
                self.modal = Modal::None;
                return;
            }
            Modal::ConfirmDelete {
                chat_id,
                message_id,
            } => {
                let (c, m) = (*chat_id, *message_id);
                self.modal = Modal::None;
                if matches!(
                    k.code,
                    crossterm::event::KeyCode::Char('y' | 'Y') | crossterm::event::KeyCode::Enter
                ) {
                    let tg = self.tg.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = tg.delete(c, m).await {
                            let _ = tx.send(AppEvent::Status(format!("delete failed: {e:#}")));
                        }
                    });
                }
                return;
            }
            Modal::None => {}
        }
        if let Some(a) = self.keys.global.lookup(&k) {
            return self.on_global(a);
        }
        match self.focus {
            Focus::Tree => {
                if let Some(a) = self.keys.tree.lookup(&k) {
                    self.on_tree(a);
                }
            }
            Focus::Messages => {
                if let Some(a) = self.keys.messages.lookup(&k) {
                    self.on_messages(a);
                }
            }
            Focus::Composer => self.on_composer_key(k),
        }
    }

    fn quit(&mut self) {
        if self.quit_at.is_some() {
            return;
        }
        self.quit_at = Some(Instant::now() + Duration::from_secs(3));
        let tg = self.tg.clone();
        tokio::spawn(async move {
            let _ = functions::close(tg.client_id).await;
        });
        if !matches!(self.auth, Auth::Ready) && !matches!(self.auth, Auth::Starting) {
            // Nothing to flush mid-login; leave right away.
            self.quit_at = Some(Instant::now());
        }
    }

    fn on_global(&mut self, a: GlobalAction) {
        match a {
            GlobalAction::FocusChats => {
                self.sidebar = true;
                self.focus = Focus::Tree;
            }
            GlobalAction::FocusMessages => self.focus = Focus::Messages,
            GlobalAction::FocusComposer => {
                if self.current.is_some() {
                    self.focus = Focus::Composer;
                }
            }
            GlobalAction::FocusNext | GlobalAction::FocusPrevious => {
                let mut order = vec![];
                if self.sidebar {
                    order.push(Focus::Tree);
                }
                order.push(Focus::Messages);
                if self.current.is_some() {
                    order.push(Focus::Composer);
                }
                let i = order.iter().position(|f| *f == self.focus).unwrap_or(0);
                let n = order.len();
                let j = if a == GlobalAction::FocusNext {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                };
                self.focus = order[j];
            }
            GlobalAction::ToggleSidebar => {
                self.sidebar = !self.sidebar;
                if !self.sidebar && self.focus == Focus::Tree {
                    self.focus = Focus::Messages;
                }
            }
            GlobalAction::Picker => self.modal = Modal::Picker(ChatPicker::new(&self.store)),
            GlobalAction::ToggleHelp => self.modal = Modal::Help,
            GlobalAction::Suspend => self.pending_interrupt = Some(Interrupt::Suspend),
            GlobalAction::Logout => {
                self.logging_out = true;
                let tg = self.tg.clone();
                tokio::spawn(async move {
                    let _ = functions::log_out(tg.client_id).await;
                });
                self.set_status("logging out…");
            }
            GlobalAction::Quit => self.quit(),
        }
    }

    // ---------------------------------------------------------------- tree

    fn on_tree(&mut self, a: TreeAction) {
        let rows = chats_tree::rows(&self.store, &self.tree);
        if rows.is_empty() {
            return;
        }
        let i = chats_tree::index_of(&rows, self.tree.selected);
        let page = self.rects.tree_inner.height.max(1) as usize;
        let set = |s: &mut Self, j: usize| s.tree.selected = Some(rows[j.min(rows.len() - 1)].id);
        match a {
            TreeAction::Up => set(self, i.saturating_sub(1)),
            TreeAction::Down => set(self, i + 1),
            TreeAction::Top => set(self, 0),
            TreeAction::Bottom => set(self, rows.len() - 1),
            TreeAction::PageUp => set(self, i.saturating_sub(page)),
            TreeAction::PageDown => set(self, i + page),
            TreeAction::Select => {
                let row = rows[i].clone();
                self.tree.selected = Some(row.id);
                self.activate_node(row.id, row.expandable);
            }
            TreeAction::CollapseParent => {
                let id = rows[i].id;
                if rows[i].expanded {
                    self.tree.expanded.remove(&id);
                } else if let Some(p) = id.parent() {
                    self.tree.expanded.remove(&p);
                    self.tree.selected = Some(p);
                }
            }
            TreeAction::CollapseAll => {
                self.tree.expanded.clear();
                // Select the top-level list the row was in.
                let mut top = rows[i].id;
                while let Some(p) = top.parent() {
                    top = p;
                }
                self.tree.selected = Some(top);
            }
            TreeAction::MoveToParent => {
                if let Some(p) = rows[i].id.parent() {
                    self.tree.selected = Some(p);
                }
            }
            TreeAction::NextUnread | TreeAction::PrevUnread => {
                if let Some(j) = chats_tree::next_unread(&rows, i, a == TreeAction::NextUnread) {
                    set(self, j);
                } else {
                    self.set_status("no unread chats");
                }
            }
            TreeAction::YankId => {
                let id = match rows[i].id {
                    NodeId::List(_) => return,
                    NodeId::Chat(_, c) => c.to_string(),
                    NodeId::Topic(_, c, t) => format!("{c}/{t}"),
                };
                self.yank(id);
            }
        }
    }

    fn activate_node(&mut self, id: NodeId, expandable: bool) {
        match id {
            NodeId::List(_) => toggle(&mut self.tree.expanded, id),
            NodeId::Chat(_, chat_id) if expandable => {
                toggle(&mut self.tree.expanded, id);
                if self.tree.expanded.contains(&id) {
                    self.load_topics(chat_id);
                }
            }
            _ => {
                if let Some(v) = id.view() {
                    self.open_view(v);
                }
            }
        }
    }

    fn load_topics(&self, chat_id: i64) {
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = tg.forum_topics(chat_id).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Topics { chat_id, result });
        });
    }

    pub fn open_view(&mut self, view: View) {
        if self.current == Some(view) {
            self.focus = Focus::Composer;
            return;
        }
        let prev = self.current.replace(view);
        let tg = self.tg.clone();
        let chat_id = view.chat_id;
        tokio::spawn(async move {
            if let Some(p) = prev
                && p.chat_id != chat_id
            {
                let _ = functions::close_chat(p.chat_id, tg.client_id).await;
            }
            let _ = functions::open_chat(chat_id, tg.client_id).await;
        });
        self.msgs = MessagesState::default();
        self.composer.clear();
        self.load_history(view);
        if self.store.is_forum(view.chat_id) && view.topic.is_none() {
            self.load_topics(view.chat_id);
        }
        self.focus = Focus::Composer;
    }

    // ---------------------------------------------------------------- messages

    fn selected_message(&self) -> Option<&Message> {
        let v = self.current?;
        self.store.message(v.chat_id, self.msgs.selected?)
    }

    fn on_messages(&mut self, a: MessagesAction) {
        let Some(view) = self.current else {
            return;
        };
        let ids: Vec<i64> = self
            .store
            .view_messages(view)
            .iter()
            .map(|m| m.id)
            .collect();
        let pos = self
            .msgs
            .selected
            .and_then(|s| ids.iter().position(|&i| i == s));
        let page = self.rects.messages_inner.height.max(1) as usize;
        let select = |s: &mut Self, i: usize| {
            if let Some(&id) = ids.get(i) {
                s.msgs.selected = Some(id);
                s.msgs.reveal = true;
            }
        };
        match a {
            MessagesAction::Up => match pos {
                Some(p) => select(self, p.saturating_sub(1)),
                None if !ids.is_empty() => select(self, ids.len() - 1),
                None => {}
            },
            MessagesAction::Down => match pos {
                Some(p) if p + 1 < ids.len() => select(self, p + 1),
                Some(_) => {
                    self.msgs.selected = None;
                    self.msgs.scroll = 0;
                }
                None => {}
            },
            MessagesAction::Top => select(self, 0),
            MessagesAction::Bottom => {
                self.msgs.selected = None;
                self.msgs.scroll = 0;
            }
            MessagesAction::ScrollUp => self.msgs.scroll += page / 2,
            MessagesAction::ScrollDown => {
                self.msgs.scroll = self.msgs.scroll.saturating_sub(page / 2)
            }
            MessagesAction::Cancel => self.msgs.selected = None,
            MessagesAction::SelectReply => {
                let Some(m) = self.selected_message() else {
                    return;
                };
                let Some(enums::MessageReplyTo::Message(r)) = &m.reply_to else {
                    return self.set_status("not a reply");
                };
                if r.chat_id != 0 && r.chat_id != view.chat_id {
                    return self.set_status("reply is in another chat");
                }
                let target = r.message_id;
                if self.store.message(view.chat_id, target).is_some() {
                    self.msgs.selected = Some(target);
                    self.msgs.reveal = true;
                } else {
                    let tg = self.tg.clone();
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        let result = tg
                            .history_around(view.chat_id, target, 20)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(AppEvent::Around {
                            view,
                            target,
                            result,
                        });
                    });
                }
            }
            MessagesAction::Reply => {
                if let Some(id) = self.msgs.selected {
                    self.composer.mode = Mode::Reply(id);
                    self.focus = Focus::Composer;
                }
            }
            MessagesAction::Edit => {
                if let Some(m) = self.selected_message() {
                    self.start_edit(m.id);
                }
            }
            MessagesAction::Delete => {
                if let Some(m) = self.selected_message() {
                    self.modal = Modal::ConfirmDelete {
                        chat_id: m.chat_id,
                        message_id: m.id,
                    };
                }
            }
            MessagesAction::Open => self.attachment(None),
            MessagesAction::Download => {
                let dir = directories::UserDirs::new()
                    .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
                    .unwrap_or_else(std::env::temp_dir);
                self.attachment(Some(dir));
            }
            MessagesAction::YankText => {
                if let Some(m) = self.selected_message() {
                    let t = messages::parts(m, &self.store)
                        .1
                        .map(|t| t.text)
                        .unwrap_or_default();
                    self.yank(t);
                }
            }
            MessagesAction::YankUrl => {
                if let Some(m) = self.selected_message() {
                    let urls = messages::parts(m, &self.store)
                        .1
                        .map(|t| crate::ui::render_text::urls(&t))
                        .unwrap_or_default();
                    match urls.into_iter().next() {
                        Some(u) => self.yank(u),
                        None => self.set_status("no link in message"),
                    }
                }
            }
            MessagesAction::YankId => {
                if let Some(id) = self.msgs.selected {
                    self.yank(id.to_string());
                }
            }
        }
    }

    fn start_edit(&mut self, id: i64) {
        let Some(view) = self.current else { return };
        let Some(m) = self.store.message(view.chat_id, id) else {
            return;
        };
        if !m.is_outgoing {
            return self.set_status("can only edit your own messages");
        }
        let text = match &m.content {
            MessageContent::MessageText(t) => t.text.clone(),
            _ => return self.set_status("only text messages can be edited"),
        };
        let md = if self.cfg.markdown.enabled {
            to_markdown(&text)
        } else {
            text.text
        };
        self.composer.set_text(&md);
        self.composer.mode = Mode::Edit(id);
        self.focus = Focus::Composer;
    }

    /// Open (`dest` None) or save the selected message's file.
    fn attachment(&mut self, dest: Option<PathBuf>) {
        let Some(m) = self.selected_message() else {
            return;
        };
        let file = match &m.content {
            MessageContent::MessagePhoto(p) => p
                .photo
                .sizes
                .iter()
                .max_by_key(|s| s.width)
                .map(|s| (s.photo.clone(), format!("photo_{}.jpg", m.id))),
            MessageContent::MessageDocument(d) => {
                Some((d.document.document.clone(), d.document.file_name.clone()))
            }
            MessageContent::MessageVideo(v) => Some((
                v.video.video.clone(),
                non_empty(&v.video.file_name, format!("video_{}.mp4", m.id)),
            )),
            MessageContent::MessageAnimation(a) => Some((
                a.animation.animation.clone(),
                non_empty(&a.animation.file_name, format!("gif_{}.mp4", m.id)),
            )),
            MessageContent::MessageAudio(a) => Some((
                a.audio.audio.clone(),
                non_empty(&a.audio.file_name, format!("audio_{}.mp3", m.id)),
            )),
            MessageContent::MessageVoiceNote(v) => {
                Some((v.voice_note.voice.clone(), format!("voice_{}.ogg", m.id)))
            }
            MessageContent::MessageVideoNote(v) => Some((
                v.video_note.video.clone(),
                format!("video_note_{}.mp4", m.id),
            )),
            MessageContent::MessageSticker(s) => {
                Some((s.sticker.sticker.clone(), format!("sticker_{}.webp", m.id)))
            }
            _ => None,
        };
        let Some((file, name)) = file else {
            return self.set_status("no attachment");
        };
        let purpose = match dest {
            None => FilePurpose::Open,
            Some(dir) => FilePurpose::Save(dir.join(sanitize(&name))),
        };
        self.set_status("downloading…");
        self.spawn_download(file.id, purpose, 32);
    }

    fn yank(&mut self, text: String) {
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => self.set_status("copied"),
            Err(e) => self.set_status(format!("clipboard: {e}")),
        }
    }

    // ---------------------------------------------------------------- composer

    fn on_composer_key(&mut self, k: KeyEvent) {
        match self.keys.composer.lookup(&k) {
            Some(ComposerAction::Send) => return self.send(),
            Some(ComposerAction::Newline) => {
                self.composer.ta.insert_newline();
                return;
            }
            Some(ComposerAction::Cancel) => {
                if self.composer.mode != Mode::Normal {
                    let was_edit = matches!(self.composer.mode, Mode::Edit(_));
                    self.composer.mode = Mode::Normal;
                    if was_edit {
                        self.composer.clear();
                    }
                } else if !self.composer.is_empty() {
                    self.composer.clear();
                } else {
                    self.focus = Focus::Messages;
                }
                return;
            }
            Some(ComposerAction::EditLast) if self.composer.is_empty() => {
                if let Some(view) = self.current {
                    let last = self
                        .store
                        .view_messages(view)
                        .iter()
                        .rev()
                        .find(|m| m.is_outgoing)
                        .map(|m| m.id);
                    if let Some(id) = last {
                        self.start_edit(id);
                    }
                }
                return;
            }
            Some(ComposerAction::OpenEditor) => {
                self.pending_interrupt = Some(Interrupt::Editor);
                return;
            }
            _ => {}
        }
        if self.composer.ta.input(k) {
            self.send_typing();
        }
    }

    fn send_typing(&mut self) {
        if !self.cfg.typing_indicator.send || !self.composer.should_send_typing() {
            return;
        }
        let Some(view) = self.current else { return };
        let tg = self.tg.clone();
        tokio::spawn(async move {
            let _ = functions::send_chat_action(
                view.chat_id,
                view.message_topic(),
                Some(enums::ChatAction::Typing),
                tg.client_id,
            )
            .await;
        });
    }

    fn send(&mut self) {
        let Some(view) = self.current else { return };
        let text = self.composer.text();
        if text.trim().is_empty() {
            return;
        }
        let mode = self.composer.mode;
        self.composer.clear();
        self.composer.last_typing_sent = None;
        self.msgs.selected = None;
        self.msgs.scroll = 0;
        let tg = self.tg.clone();
        let tx = self.tx.clone();
        let markdown = self.cfg.markdown.enabled;
        tokio::spawn(async move {
            let formatted = tg.formatted(text, markdown).await;
            let res = match mode {
                Mode::Edit(id) => tg.edit_text(view.chat_id, id, formatted).await,
                Mode::Reply(id) => tg.send_text(view, Some(id), formatted).await,
                Mode::Normal => tg.send_text(view, None, formatted).await,
            };
            if let Err(e) = res {
                let _ = tx.send(AppEvent::Status(format!("send failed: {e:#}")));
            }
        });
    }

    /// Text currently in the composer, for `$EDITOR`.
    pub fn composer_text(&self) -> String {
        self.composer.text()
    }

    pub fn set_composer_text(&mut self, text: &str) {
        let mode = self.composer.mode;
        self.composer.set_text(text.trim_end_matches('\n'));
        self.composer.mode = mode;
        self.focus = Focus::Composer;
        self.dirty = true;
    }

    // ---------------------------------------------------------------- picker

    fn on_picker_key(&mut self, k: KeyEvent) {
        let Modal::Picker(p) = &mut self.modal else {
            return;
        };
        match self.keys.picker.lookup(&k) {
            Some(PickerAction::Up) => p.move_by(-1),
            Some(PickerAction::Down) => p.move_by(1),
            Some(PickerAction::Cancel) => self.modal = Modal::None,
            Some(PickerAction::Select) => {
                let v = p.current();
                self.modal = Modal::None;
                if let Some(v) = v {
                    self.reveal_in_tree(v);
                    self.open_view(v);
                }
            }
            None => {
                use crossterm::event::KeyCode;
                match k.code {
                    KeyCode::Backspace => {
                        p.query.pop();
                        p.refilter();
                    }
                    KeyCode::Char(c)
                        if !k.modifiers.intersects(
                            crossterm::event::KeyModifiers::CONTROL
                                | crossterm::event::KeyModifiers::ALT,
                        ) =>
                    {
                        p.query.push(c);
                        p.refilter();
                    }
                    _ => {}
                }
            }
        }
    }

    fn reveal_in_tree(&mut self, v: View) {
        let list = self
            .store
            .chats
            .get(&v.chat_id)
            .and_then(|c| c.positions.first().map(|p| ListKey::from_list(&p.list)))
            .unwrap_or(ListKey::Main);
        self.tree.expanded.insert(NodeId::List(list));
        let node = match v.topic {
            Some(t) => {
                self.tree.expanded.insert(NodeId::Chat(list, v.chat_id));
                NodeId::Topic(list, v.chat_id, t)
            }
            None => NodeId::Chat(list, v.chat_id),
        };
        self.tree.selected = Some(node);
    }

    // ---------------------------------------------------------------- login

    fn on_login_key(&mut self, k: KeyEvent) {
        use crossterm::event::KeyCode;
        if self.login_busy {
            return;
        }
        match (&self.auth, k.code) {
            (Auth::Phone, KeyCode::Tab) => {
                self.login_busy = true;
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        functions::request_qr_code_authentication(vec![], tg.client_id).await
                    {
                        let _ = tx.send(AppEvent::AuthError(e.message));
                    }
                });
            }
            (
                Auth::Phone | Auth::Code(_) | Auth::Password(_) | Auth::Email | Auth::EmailCode,
                KeyCode::Enter,
            ) => {
                let value = self.login_input.lines().join("").trim().to_string();
                if value.is_empty() {
                    return;
                }
                self.login_busy = true;
                self.login_error = None;
                let auth = self.auth.clone();
                let tg = self.tg.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let id = tg.client_id;
                    let res = match auth {
                        Auth::Phone => {
                            functions::set_authentication_phone_number(value, None, id).await
                        }
                        Auth::Code(_) => functions::check_authentication_code(value, id).await,
                        Auth::Password(_) => {
                            functions::check_authentication_password(value, id).await
                        }
                        Auth::Email => functions::set_authentication_email_address(value, id).await,
                        Auth::EmailCode => {
                            functions::check_authentication_email_code(
                                enums::EmailAddressAuthentication::Code(
                                    tdlib_rs::types::EmailAddressAuthenticationCode { code: value },
                                ),
                                id,
                            )
                            .await
                        }
                        _ => Ok(()),
                    };
                    // Success arrives as a new authorization state.
                    if let Err(e) = res {
                        let _ = tx.send(AppEvent::AuthError(e.message));
                    }
                });
            }
            (
                Auth::Qr(_) | Auth::Starting | Auth::Registration | Auth::Closing | Auth::Ready,
                _,
            ) => {}
            _ => {
                self.login_input.input(k);
            }
        }
    }

    // ---------------------------------------------------------------- mouse

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        if !matches!(self.auth, Auth::Ready) || !matches!(self.modal, Modal::None) {
            return;
        }
        let pos = ratatui::layout::Position {
            x: m.column,
            y: m.row,
        };
        let r = self.rects;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                if self.sidebar && r.tree.contains(pos) {
                    self.focus = Focus::Tree;
                    if r.tree_inner.contains(pos) {
                        let rows = chats_tree::rows(&self.store, &self.tree);
                        let idx = self.tree.offset + (m.row - r.tree_inner.y) as usize;
                        if let Some(row) = rows.get(idx).cloned() {
                            self.tree.selected = Some(row.id);
                            self.activate_node(row.id, row.expandable);
                        }
                    }
                } else if r.messages.contains(pos) {
                    self.focus = Focus::Messages;
                    if r.messages_inner.contains(pos)
                        && m.row >= r.messages_inner.y + r.messages_pad
                    {
                        let line =
                            r.messages_top + (m.row - r.messages_inner.y - r.messages_pad) as usize;
                        if let Some(&(id, _, _)) = self
                            .last_ranges
                            .iter()
                            .find(|(_, a, b)| line >= *a && line < *b)
                        {
                            self.msgs.selected = Some(id);
                        }
                    }
                } else if r.composer.contains(pos) && self.current.is_some() {
                    self.focus = Focus::Composer;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.dirty = true;
                let up = matches!(m.kind, MouseEventKind::ScrollUp);
                if r.messages.contains(pos) {
                    if up {
                        self.msgs.scroll += 3;
                    } else {
                        self.msgs.scroll = self.msgs.scroll.saturating_sub(3);
                    }
                } else if self.sidebar && r.tree.contains(pos) {
                    self.on_tree(if up { TreeAction::Up } else { TreeAction::Down });
                }
            }
            _ => {}
        }
    }
}

fn toggle(set: &mut std::collections::HashSet<NodeId>, id: NodeId) {
    if !set.remove(&id) {
        set.insert(id);
    }
}

fn non_empty(s: &str, fallback: String) -> String {
    if s.is_empty() {
        fallback
    } else {
        s.to_string()
    }
}

/// `dir/name.ext`, or `dir/name (1).ext` and so on if it exists.
fn unique_path(p: &std::path::Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let dir = p.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .expect("some free name")
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn open_path(path: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!("{opener}: {e}");
    }
}

/// Re-creates editable markdown from entities (inverse of TDLib `parseMarkdown`).
pub fn to_markdown(ft: &tdlib_rs::types::FormattedText) -> String {
    use tdlib_rs::enums::TextEntityType as T;
    let units: Vec<u16> = ft.text.encode_utf16().collect();
    // (utf16 position, is_close, marker) inserts.
    let mut marks: Vec<(usize, bool, String)> = Vec::new();
    for e in &ft.entities {
        let (open, close) = match &e.r#type {
            T::Bold => ("**".to_string(), "**".to_string()),
            T::Italic => ("__".into(), "__".into()),
            T::Strikethrough => ("~~".into(), "~~".into()),
            T::Spoiler => ("||".into(), "||".into()),
            T::Code => ("`".into(), "`".into()),
            T::Pre | T::PreCode(_) => {
                let lang = if let T::PreCode(p) = &e.r#type {
                    p.language.clone()
                } else {
                    String::new()
                };
                // TDLib keeps the block's trailing newline inside the entity.
                let end = (e.offset + e.length).max(0) as usize;
                let ends_nl = end > 0 && units.get(end - 1) == Some(&(b'\n' as u16));
                (
                    format!("```{lang}\n"),
                    if ends_nl {
                        "```".into()
                    } else {
                        "\n```".into()
                    },
                )
            }
            T::TextUrl(u) => ("[".into(), format!("]({})", u.url)),
            _ => continue,
        };
        let a = e.offset.max(0) as usize;
        let b = (e.offset + e.length).max(0) as usize;
        marks.push((a, false, open));
        marks.push((b, true, close));
    }
    // Closes before opens at the same spot; later opens close first.
    marks.sort_by(|x, y| x.0.cmp(&y.0).then(y.1.cmp(&x.1)));
    let mut out = String::new();
    let mut last = 0;
    for (pos, _, mark) in marks {
        let pos = pos.min(units.len());
        out.push_str(&String::from_utf16_lossy(&units[last..pos]));
        out.push_str(&mark);
        last = pos;
    }
    out.push_str(&String::from_utf16_lossy(&units[last..]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::TextEntityType;
    use tdlib_rs::types::{FormattedText, TextEntity};

    use crate::testutil::{self, BASE_DATE};
    use crate::ui::image::{Entry, Images};
    use crate::ui::tests::render;
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::time::Duration;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn ch(c: char) -> KeyEvent {
        let mods = if c.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        key(KeyCode::Char(c), mods)
    }

    #[tokio::test]
    async fn focus_keys_in_enhanced_and_legacy_terminals() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.on_key(key(KeyCode::Char('i'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Composer);
        app.on_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Tree);
        app.on_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Messages);
        app.on_key(key(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Composer);

        let (mut legacy, _rx2) = testutil::app(Images::disabled(), false);
        legacy.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        legacy.focus = Focus::Tree;
        legacy.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        legacy.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(legacy.focus, Focus::Tree, "Backspace/Tab never move focus");
        legacy.on_key(key(KeyCode::Char('i'), KeyModifiers::ALT));
        assert_eq!(legacy.focus, Focus::Composer);
    }

    #[tokio::test]
    async fn tree_moves_jumps_to_unread_and_opens() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.focus = Focus::Tree;
        // Rows: All chats, Family, Muted news, Saved Messages.
        app.on_key(ch('j'));
        assert_eq!(app.tree.selected, Some(NodeId::Chat(ListKey::Main, 10)));
        app.on_key(ch('J'));
        assert_eq!(
            app.tree.selected,
            Some(NodeId::Chat(ListKey::Main, 11)),
            "next unread"
        );
        app.on_key(ch('G'));
        assert_eq!(app.tree.selected, Some(NodeId::Chat(ListKey::Main, 12)));
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.current,
            Some(View {
                chat_id: 12,
                topic: None
            })
        );
        assert_eq!(app.focus, Focus::Composer);
        app.focus = Focus::Tree;
        app.on_key(ch('_'));
        assert_eq!(app.tree.selected, Some(NodeId::List(ListKey::Main)));
        assert!(app.tree.expanded.is_empty());
    }

    #[tokio::test]
    async fn picker_filters_and_opens() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.on_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert!(matches!(app.modal, Modal::Picker(_)));
        for c in "svd".chars() {
            app.on_key(ch(c));
        }
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(
            app.current,
            Some(View {
                chat_id: 12,
                topic: None
            })
        );
        assert_eq!(
            app.tree.selected,
            Some(NodeId::Chat(ListKey::Main, 12)),
            "picker reveals the chat in the tree"
        );
    }

    #[tokio::test]
    async fn composer_up_edits_last_own_message_and_esc_cancels() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.focus = Focus::Composer;
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.composer.mode, Mode::Edit(102));
        assert_eq!(app.composer.text(), "**Great** thanks");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.composer.mode, Mode::Normal);
        assert!(app.composer.is_empty());
        // Typing goes into the composer, even letters bound elsewhere.
        for c in "jk".chars() {
            app.on_key(ch(c));
        }
        assert_eq!(app.composer.text(), "jk");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.composer.is_empty(), "esc clears text");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            app.focus,
            Focus::Messages,
            "esc on empty composer leaves it"
        );
    }

    #[tokio::test]
    async fn messages_select_reply_and_delete_confirm() {
        let (mut app, _rx) = testutil::app(Images::disabled(), true);
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });
        app.focus = Focus::Messages;
        app.on_key(ch('k'));
        assert_eq!(
            app.msgs.selected,
            Some(102),
            "k from nothing selects the newest"
        );
        app.on_key(ch('s'));
        assert_eq!(
            app.msgs.selected,
            Some(100),
            "s jumps to the replied message"
        );
        app.on_key(ch('d'));
        assert!(matches!(
            app.modal,
            Modal::ConfirmDelete {
                chat_id: 10,
                message_id: 100
            }
        ));
        app.on_key(ch('n'));
        assert!(matches!(app.modal, Modal::None));
        app.on_key(ch('R'));
        assert_eq!(app.composer.mode, Mode::Reply(100));
        assert_eq!(app.focus, Focus::Composer);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn photo_downloads_encode_and_render_inline() {
        let dir = std::env::temp_dir().join(format!("telegordo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("p.png");
        ::image::RgbImage::from_fn(64, 48, |x, y| {
            ::image::Rgb([(x * 4) as u8, (y * 5) as u8, 200])
        })
        .save(&png)
        .unwrap();
        let path = png.to_string_lossy().into_owned();
        let images = Images {
            picker: Some(ratatui_image::picker::Picker::halfblocks()),
            max_cols: 40,
            max_rows: 12,
            entries: Default::default(),
        };
        let (mut app, mut rx) = testutil::app(images, true);
        app.store.insert_message(testutil::message(
            103,
            10,
            2,
            BASE_DATE + 120,
            testutil::photo_content(77, &path, 64, 48),
            None,
        ));
        app.store.files.insert(77, testutil::file(77, &path, true));
        app.current = Some(View {
            chat_id: 10,
            topic: None,
        });

        let before = render(&mut app, 90, 30);
        assert!(before.contains("[Photo]"), "placeholder while loading");
        app.after_draw();
        let ev = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(e @ AppEvent::ImageReady { .. }) => break e,
                    Some(_) => continue,
                    None => panic!("channel closed"),
                }
            }
        })
        .await
        .expect("image encoded");
        app.on_app_event(ev);
        assert!(matches!(app.images.entries.get(&77), Some(Entry::Ready(_))));
        let after = render(&mut app, 90, 30);
        println!("{after}");
        assert!(
            after.contains('▀') || after.contains('▄'),
            "halfblock pixels drawn"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tdlib_parses_the_documented_markdown() {
        use tdlib_rs::enums::TextEntityType as T;
        let tg = testutil::shared_tg();
        let src = "**b** __i__ ~~s~~ ||p|| `c` [t](https://example.com)\n```rust\nfn x() {}\n```";
        let f = tokio::time::timeout(Duration::from_secs(10), tg.formatted(src.into(), true))
            .await
            .expect("parse_markdown answered before login");
        let kinds: Vec<&T> = f.entities.iter().map(|e| &e.r#type).collect();
        for want in [
            "Bold",
            "Italic",
            "Strikethrough",
            "Spoiler",
            "Code",
            "TextUrl",
            "PreCode",
        ] {
            assert!(
                kinds.iter().any(|k| format!("{k:?}").starts_with(want)),
                "missing {want} in {kinds:?}"
            );
        }
        assert!(f.text.starts_with("b i s p c t"));
        // Editing round-trip: our markdown parses back to the same entities.
        let again = tg.formatted(to_markdown(&f), true).await;
        assert_eq!(again.text, f.text);
        assert_eq!(again.entities, f.entities);
    }

    #[test]
    fn markdown_round_trip_for_editing() {
        let ft = FormattedText {
            text: "hi bold and 😀code".into(),
            entities: vec![
                TextEntity {
                    offset: 3,
                    length: 4,
                    r#type: TextEntityType::Bold,
                },
                TextEntity {
                    offset: 14,
                    length: 4,
                    r#type: TextEntityType::Code,
                },
            ],
        };
        assert_eq!(to_markdown(&ft), "hi **bold** and 😀`code`");
    }
}
