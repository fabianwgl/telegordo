# Roadmap

Version 1 covers text chat and inline images. Everything below is planned but
not built. Each item has a short note on how to build it with the TDLib API
that telegordo already uses (tdlib-rs 1.4.0, TDLib 1.8.61).

## Chat UI (discordo parity)

- [ ] **Members pane** on the right at 20% width, toggled with `alt+m` and focused with `ctrl+y`.
  - How:
    - Groups: `getSupergroupMembers` / `getBasicGroupFullInfo` → members.
    - Private chats: show the other user's profile.
    - Status dots from `updateUserStatus`.
  - Layout slot: in `ui::draw`, `row(column(messages, composer), members)`, as in discordo `internal/ui/chat/model.go`.
- [ ] **Reactions**: show them under a message (`Message.interaction_info.reactions`), and add one with `a` through an emoji picker modal.
  - How: `addMessageReaction`, `getMessageAvailableReactions`.
- [ ] **`@` and `:` autocomplete** in the composer.
  - How:
    - Mentions: `searchChatMembers`, inserted as `[Name](tg://user?id=…)` or as `MentionName` entities.
    - Emoji: `searchEmojis`.
- [ ] **Unread marker line** ("new messages") at `ChatInfo.last_read_inbox` when a chat opens.
- [ ] **Pinned message bar** at the top of the message list.
  - How: `getChatPinnedMessage`, `updateMessageIsPinned`.
- [ ] **Full forum view**: open a forum chat as a list of topics with their last message, not just tree children. Keep topic unread counts current.
  - How: `updateForumTopic`, plus refreshing `getForumTopics` per open chat.
- [ ] **Saved Messages topics** and **channel direct messages**.
  - How: `MessageTopic::SavedMessages` / `DirectMessages`.
- [ ] **In-chat search** with `/` in the messages pane.
  - How: `searchChatMessages`, then jump through results with `history_around`.
- [ ] **Global message search** in the picker, below the chat matches.
  - How: `searchMessages`.
- [ ] **History gaps**: the cache pages backwards only. If the app misses updates (a long sleep or disconnect), newer messages can arrive without the ones in between. On `updateConnectionState → Ready`, re-fetch the newest page of the open chat and merge it.
- [ ] **Message layout cache**: keep each message's wrapped lines until its width or `edit_date` changes. Long histories currently re-layout on every draw.

## Media

- [ ] **File and photo upload** with `ctrl+\` opening a file picker.
  - How: `InputMessageContent::InputMessageDocument` / `InputMessagePhoto` with `InputFile::Local`.
- [ ] **Paste an image from the clipboard** with `ctrl+v`.
  - How: `arboard::Clipboard::get_image`, save to a temp PNG, then send it as a photo.
- [ ] **Video, GIF and document thumbnails** drawn inline, through the same path as photos.
  - How: `Thumbnail.file` when `ThumbnailFormat` is Jpeg/Png/Webp, else `Minithumbnail.data` (base64 JPEG).
- [ ] **Animated stickers** (`.tgs` Lottie, `.webm`) as a still frame: use `Sticker.thumbnail`.
- [ ] **Voice messages and audio**: `p` play/pause, `h`/`l` seek.
  - How: download the OGG/Opus file, then play it with `rodio` + an opus decoder. tgt's `ogg_opus.rs` is a reference.
- [ ] **Video notes**: show the thumbnail and open the file externally.
- [ ] **Albums**: group messages that share a `media_album_id` into one block.
- [ ] **Link previews**: render `MessageText.link_preview` as an embed box (site, title, description).

## Messaging

- [ ] **Forward** messages (`f`), with a chat picker for the target.
  - How: `forwardMessages`.
- [ ] **Reply quoting** of part of a message.
  - How: `InputTextQuote`.
- [ ] **Drafts**: save the composer text with `setChatDraftMessage` on chat switch, and restore it from `Chat.draft_message`.
- [ ] **Polls**: show options and results; vote.
  - How: `setPollAnswer`.
- [ ] **Scheduled messages and silent send.**
  - How: `MessageSendOptions`.
- [ ] **Mark chat as read/unread** from the tree.
  - How: `toggleChatIsMarkedAsUnread`, `readAllChatMentions`.
- [ ] **Mute and unmute** from the tree.
  - How: `setChatNotificationSettings`.
- [ ] **Pin, archive and move chats between folders.**
  - How: `toggleChatIsPinned`, `addChatToList`.
- [ ] **Join chats from links** (`t.me/...`).
  - How: `joinChatByInviteLink`, `searchPublicChat`.

## Accounts and security

- [ ] **Secret chats**: set `use_secret_chats = true` and handle `ChatType::Secret` state and key fingerprints.
- [ ] **Multiple accounts**: one TDLib client per account (`create_client`), each with its own data dir and keyring entry, plus an account switcher.
- [ ] **Proxy settings** in the config: SOCKS5 and MTProto.
  - How: `addProxy`.
- [ ] **Active sessions list** and terminating sessions.
  - How: `getActiveSessions`, `terminateSession`.
- [ ] **Registering a new account** at the `WaitRegistration` step.
  - How: `registerUser`.

## Look and config

- [ ] **Themes**: full style tables per widget (`theme.tree.*`, `theme.messages.*`), like discordo's `active_set` / `active_style`.
- [ ] **Config reload** without restarting.
- [ ] **Custom date and time formats** per locale; 24h detection.
- [ ] **Custom emoji**: render `CustomEmoji` entities as their fallback emoji (currently plain text).

## Not planned

- Voice and video calls.
- Stories.
- Bots' inline keyboards beyond plain text (could come later as numbered buttons).
