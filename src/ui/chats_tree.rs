//! Sidebar tree: chat lists (All chats, folders, Archive) → chats → forum topics.

use std::collections::HashSet;

use crate::tg::View;
use crate::tg::store::{ListKey, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeId {
    List(ListKey),
    Chat(ListKey, i64),
    Topic(ListKey, i64, i32),
}

impl NodeId {
    pub fn parent(self) -> Option<NodeId> {
        match self {
            NodeId::List(_) => None,
            NodeId::Chat(l, _) => Some(NodeId::List(l)),
            NodeId::Topic(l, c, _) => Some(NodeId::Chat(l, c)),
        }
    }

    pub fn view(self) -> Option<View> {
        match self {
            NodeId::List(_) => None,
            NodeId::Chat(_, c) => Some(View {
                chat_id: c,
                topic: None,
            }),
            NodeId::Topic(_, c, t) => Some(View {
                chat_id: c,
                topic: Some(t),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub id: NodeId,
    pub depth: usize,
    pub label: String,
    pub unread: i32,
    pub mentions: i32,
    pub muted: bool,
    pub expandable: bool,
    pub expanded: bool,
}

#[derive(Debug)]
pub struct TreeState {
    pub selected: Option<NodeId>,
    pub expanded: HashSet<NodeId>,
    pub offset: usize,
}

impl Default for TreeState {
    fn default() -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(NodeId::List(ListKey::Main));
        Self {
            selected: None,
            expanded,
            offset: 0,
        }
    }
}

/// Chat lists in Telegram's order: folders with "All chats" at its position, Archive last.
pub fn lists(store: &Store) -> Vec<(ListKey, String)> {
    let mut v: Vec<(ListKey, String)> = store
        .folders
        .iter()
        .map(|(id, name)| (ListKey::Folder(*id), name.clone()))
        .collect();
    let pos = store.main_list_position.min(v.len());
    v.insert(pos, (ListKey::Main, "All chats".into()));
    if !store.chats_in(ListKey::Archive).is_empty() {
        v.push((ListKey::Archive, "Archive".into()));
    }
    v
}

/// Visible rows given expand state.
pub fn rows(store: &Store, state: &TreeState) -> Vec<Row> {
    let mut out = Vec::new();
    for (list, name) in lists(store) {
        let list_id = NodeId::List(list);
        let chats = store.chats_in(list);
        let expanded = state.expanded.contains(&list_id);
        out.push(Row {
            id: list_id,
            depth: 0,
            label: name,
            unread: chats
                .iter()
                .filter(|c| c.has_unread() && !c.muted())
                .count() as i32,
            mentions: 0,
            muted: false,
            expandable: true,
            expanded,
        });
        if !expanded {
            continue;
        }
        for c in chats {
            let id = NodeId::Chat(list, c.id);
            let forum = store.is_forum(c.id);
            let expanded = forum && state.expanded.contains(&id);
            out.push(Row {
                id,
                depth: 1,
                label: c.title.clone(),
                unread: if c.marked_unread && c.unread == 0 {
                    1
                } else {
                    c.unread
                },
                mentions: c.unread_mentions,
                muted: c.muted(),
                expandable: forum,
                expanded,
            });
            if expanded {
                for t in store.topics_of(c.id) {
                    out.push(Row {
                        id: NodeId::Topic(list, c.id, t.info.forum_topic_id),
                        depth: 2,
                        label: format!("# {}", t.info.name),
                        unread: t.unread,
                        mentions: 0,
                        muted: t.mute_for > 0 || c.muted(),
                        expandable: false,
                        expanded: false,
                    });
                }
            }
        }
    }
    out
}

pub fn index_of(rows: &[Row], id: Option<NodeId>) -> usize {
    id.and_then(|id| rows.iter().position(|r| r.id == id))
        .unwrap_or(0)
}

/// Next row after `from` (wrapping) whose chat or topic has unread messages.
pub fn next_unread(rows: &[Row], from: usize, forward: bool) -> Option<usize> {
    let n = rows.len();
    if n == 0 {
        return None;
    }
    (1..=n)
        .map(|k| {
            if forward {
                (from + k) % n
            } else {
                (from + n - k) % n
            }
        })
        .find(|&i| !matches!(rows[i].id, NodeId::List(_)) && rows[i].unread > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tg::store::{ChatInfo, ChatKind};
    use tdlib_rs::types::ChatPosition;

    fn chat(store: &mut Store, id: i64, title: &str, list: ListKey, order: i64, unread: i32) {
        store.chats.insert(
            id,
            ChatInfo {
                id,
                title: title.into(),
                kind: ChatKind::BasicGroup,
                positions: vec![ChatPosition {
                    list: list.to_list(),
                    order,
                    is_pinned: false,
                    source: None,
                }],
                unread,
                unread_mentions: 0,
                marked_unread: false,
                last_read_inbox: 0,
                mute_for: 0,
            },
        );
    }

    #[test]
    fn rows_are_ordered_and_collapsible() {
        let mut store = Store {
            folders: vec![(7, "Work".into())],
            main_list_position: 0,
            ..Default::default()
        };
        chat(&mut store, 1, "old", ListKey::Main, 10, 0);
        chat(&mut store, 2, "new", ListKey::Main, 20, 3);
        let mut st = TreeState::default();
        let r = rows(&store, &st);
        let labels: Vec<_> = r.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["All chats", "new", "old", "Work"]);
        assert_eq!(r[0].unread, 1, "one unread unmuted chat");

        st.expanded.clear();
        assert_eq!(rows(&store, &st).len(), 2);
    }

    #[test]
    fn unread_navigation_wraps_and_skips_lists() {
        let mut store = Store::default();
        chat(&mut store, 1, "a", ListKey::Main, 30, 1);
        chat(&mut store, 2, "b", ListKey::Main, 20, 0);
        chat(&mut store, 3, "c", ListKey::Main, 10, 2);
        let r = rows(&store, &TreeState::default());
        // rows: All chats, a, b, c
        assert_eq!(next_unread(&r, 1, true), Some(3));
        assert_eq!(next_unread(&r, 3, true), Some(1), "wraps past the list row");
        assert_eq!(next_unread(&r, 1, false), Some(3));
    }

    #[test]
    fn parents() {
        let t = NodeId::Topic(ListKey::Main, 5, 9);
        assert_eq!(t.parent(), Some(NodeId::Chat(ListKey::Main, 5)));
        assert_eq!(
            t.view(),
            Some(View {
                chat_id: 5,
                topic: Some(9)
            })
        );
    }
}
