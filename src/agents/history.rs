//! Paging a session's history for `read` (DESIGN.md §5): the query, the page, and how a
//! page is cut out of an agent's records.

use crate::model::{Error, ErrorCode, Message, Page, Phase, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What `read` returns: the newest `limit` messages, or the newest before the page a
/// previous read's `next_cursor` names (DESIGN.md §5).
pub struct ReadQuery {
    pub limit: usize,
    /// The cursor: an item id, or a `Boundary` for Codex and OpenCode.
    pub before: Option<String>,
    /// Every message; the default is prompts and final replies.
    pub all: bool,
}

impl ReadQuery {
    pub fn shows(&self, m: &Message) -> bool {
        self.all || matches!(m.phase, Phase::Prompt | Phase::Final)
    }
}

pub struct ReadPage {
    pub messages: Page<Message>,
    /// The agent's raw records covering the same span (Codex turns, Claude and Antigravity
    /// transcript lines, Grok `updates.jsonl` lines, OpenCode message rows, pi session
    /// entries), for `--raw`.
    pub raw: Vec<Value>,
}

/// One page cut out of a history whose messages are tagged with the index of the record
/// they came from, oldest first.
pub struct Cut {
    pub messages: Vec<(usize, Message)>,
    /// The records the page spans: from the first kept message's record through the
    /// cursor's record or the newest kept message's, whichever is later, else to the end.
    pub records: std::ops::Range<usize>,
    /// Shown messages exist before the page.
    pub more: bool,
}

impl Cut {
    /// The message the next page ends before, when there is a next page.
    pub fn cursor_item(&self) -> Option<&(usize, Message)> {
        self.more.then(|| self.messages.first()).flatten()
    }
}

/// Cuts the page `q` asks for. `records` is the number of records the messages came from
/// and `older` whether records exist before them (a remote history fetched newest first
/// stops once the page is full).
pub fn cut(
    q: &ReadQuery,
    messages: Vec<(usize, Message)>,
    records: usize,
    older: bool,
) -> Result<Cut> {
    let end = match &q.before {
        Some(c) => messages
            .iter()
            .position(|(_, m)| m.item_id == *c)
            .ok_or_else(|| Error::new(ErrorCode::Precondition, format!("unknown cursor {c}")))?,
        None => messages.len(),
    };
    let cursor_record = messages.get(end).map_or(records, |(i, _)| *i);
    let mut shown: Vec<_> = messages
        .into_iter()
        .take(end)
        .filter(|(_, m)| q.shows(m))
        .collect();
    let page = shown.split_off(shown.len().saturating_sub(q.limit));
    let start = page.first().map_or(cursor_record, |(i, _)| *i);
    let end = page
        .last()
        .map_or(cursor_record, |(i, _)| cursor_record.max(i + 1));
    Ok(Cut {
        messages: page,
        records: start..end,
        more: older || !shown.is_empty(),
    })
}

/// `read` cursor of a remote history: the item a page starts with and the agent's cursor
/// that fetched the page holding it, so the next read refetches at most one extra page.
#[derive(Serialize, Deserialize)]
pub struct Boundary {
    pub item: String,
    pub page: Option<String>,
}

impl Boundary {
    pub fn parse(cursor: &str) -> Result<Boundary> {
        serde_json::from_str(cursor)
            .map_err(|_| Error::new(ErrorCode::Precondition, format!("invalid cursor {cursor}")))
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

/// The first non-blank line of `s`, trimmed and cut to `n` characters.
pub fn first_line(s: &str, n: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n).collect::<String>())
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;

    fn tagged(items: &[(usize, Role, Phase)]) -> Vec<(usize, Message)> {
        items
            .iter()
            .map(|&(record, role, phase)| {
                (
                    record,
                    Message {
                        turn_id: String::new(),
                        item_id: format!("{record}-{phase}"),
                        role,
                        phase,
                        text: String::new(),
                        from: None,
                        timestamp: None,
                    },
                )
            })
            .collect()
    }

    /// Two turns, the second one a tool call between the prompt and the reply, over
    /// five records; records 0 and 4 hold no message (a hidden marker at each end).
    const HISTORY: &[(usize, Role, Phase)] = &[
        (1, Role::User, Phase::Prompt),
        (1, Role::Assistant, Phase::Final),
        (2, Role::User, Phase::Prompt),
        (3, Role::Assistant, Phase::Other),
        (3, Role::Assistant, Phase::Final),
    ];

    fn ids(c: &Cut) -> Vec<&str> {
        c.messages.iter().map(|(_, m)| m.item_id.as_str()).collect()
    }

    #[test]
    fn read_pages_newest_first_counting_shown_messages() {
        let q = ReadQuery {
            limit: 2,
            before: None,
            all: false,
        };
        // The newest two shown messages skip the tool call; the raw span runs to the end.
        let c = cut(&q, tagged(HISTORY), 5, false).unwrap();
        assert_eq!(ids(&c), ["2-prompt", "3-final"]);
        assert_eq!((c.records.clone(), c.more), (2..5, true));
        let older = ReadQuery {
            before: Some("2-prompt".into()),
            ..q
        };
        let c = cut(&older, tagged(HISTORY), 5, false).unwrap();
        assert_eq!(ids(&c), ["1-prompt", "1-final"]);
        assert_eq!((c.records.clone(), c.more), (1..2, false));
        // With every message, a page can end inside a record, which the raw span then
        // includes; a remote history that may hold older records reports more.
        let all = ReadQuery {
            limit: 2,
            before: Some("3-final".into()),
            all: true,
        };
        let c = cut(&all, tagged(HISTORY), 5, true).unwrap();
        assert_eq!(ids(&c), ["2-prompt", "3-other"]);
        assert_eq!((c.records.clone(), c.more), (2..4, true));
        assert!(cut(&older, tagged(&HISTORY[3..]), 5, false).is_err());
    }
}
