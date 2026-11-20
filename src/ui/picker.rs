//! `ctrl+k` quick switcher: fuzzy search over chats and loaded topics.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::tg::View;
use crate::tg::store::{ListKey, Store};

pub struct PickerItem {
    pub label: String,
    pub view: View,
}

pub struct ChatPicker {
    pub query: String,
    pub items: Vec<PickerItem>,
    /// Indexes into `items`, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
    matcher: Matcher,
}

impl ChatPicker {
    pub fn new(store: &Store) -> Self {
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for list in [ListKey::Main, ListKey::Archive] {
            for c in store.chats_in(list) {
                if !seen.insert(c.id) {
                    continue;
                }
                items.push(PickerItem {
                    label: c.title.clone(),
                    view: View {
                        chat_id: c.id,
                        topic: None,
                    },
                });
                for t in store.topics_of(c.id) {
                    items.push(PickerItem {
                        label: format!("{} › {}", c.title, t.info.name),
                        view: View {
                            chat_id: c.id,
                            topic: Some(t.info.forum_topic_id),
                        },
                    });
                }
            }
        }
        let mut p = Self {
            query: String::new(),
            matches: (0..items.len()).collect(),
            items,
            selected: 0,
            matcher: Matcher::new(Config::DEFAULT),
        };
        p.refilter();
        p
    }

    pub fn refilter(&mut self) {
        self.selected = 0;
        if self.query.trim().is_empty() {
            self.matches = (0..self.items.len()).collect();
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                pattern
                    .score(Utf32Str::new(&it.label, &mut buf), &mut self.matcher)
                    .map(|s| (i, s))
            })
            .collect();
        // Higher score first; ties keep Telegram's chat order.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.matches = scored.into_iter().map(|(i, _)| i).collect();
    }

    pub fn current(&self) -> Option<View> {
        self.matches.get(self.selected).map(|&i| self.items[i].view)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::store::{ChatInfo, ChatKind};
    use tdlib_rs::types::ChatPosition;

    #[test]
    fn fuzzy_finds_chats() {
        let mut store = Store::default();
        for (id, title, order) in [
            (1, "Family group", 3),
            (2, "Work stuff", 2),
            (3, "Saved Messages", 1),
        ] {
            store.chats.insert(
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
                    unread: 0,
                    unread_mentions: 0,
                    marked_unread: false,
                    last_read_inbox: 0,
                    mute_for: 0,
                    use_default_mute_for: false,
                },
            );
        }
        let mut p = ChatPicker::new(&store);
        assert_eq!(p.matches.len(), 3);
        p.query = "wrk".into();
        p.refilter();
        assert_eq!(p.current().map(|v| v.chat_id), Some(2));
        p.query = "zzzz".into();
        p.refilter();
        assert!(p.current().is_none());
    }
}
