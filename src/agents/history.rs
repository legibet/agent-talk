//! Paging a session's history for `read` (DESIGN.md §5): the query, the page, and how a
//! page is cut out of an agent's records.

use crate::model::{Error, ErrorCode, Message, Page, Phase, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What `read` returns: the newest `limit` messages, or the newest before the page a
/// previous read's `next_cursor` names (DESIGN.md §5).
pub struct ReadQuery {
    pub limit: usize,
    /// The cursor a previous read returned: an item id, or a `Boundary` for Codex and
    /// OpenCode.
    pub cursor: Option<String>,
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

/// Cuts the page `q` asks for, ending before the item `before` names. `records` is the
/// number of records the messages came from and `older` whether records exist before them
/// (a remote history fetched newest first stops once the page is full).
pub fn cut(
    q: &ReadQuery,
    before: Option<&str>,
    messages: Vec<(usize, Message)>,
    records: usize,
    older: bool,
) -> Result<Cut> {
    let end = match before {
        Some(c) => messages
            .iter()
            .position(|(_, m)| m.item_id == c)
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
struct Boundary {
    item: String,
    page: Option<String>,
}

/// A remote history (Codex turns, OpenCode message rows) fetched newest first, page by
/// page, until it holds the page `read` asks for.
pub struct RemoteHistory {
    /// Records oldest first.
    records: Vec<Value>,
    /// For each record, the agent cursor that fetched its page.
    fetched_with: Vec<Option<String>>,
    /// The agent cursor of the next older page, when one exists.
    older: Option<String>,
    /// The item the page ends before, from the `Boundary` the caller passed.
    before: Option<String>,
}

impl RemoteHistory {
    /// `fetch` returns the page an agent cursor names (the newest without one), records
    /// newest first, and the cursor of the next older page. `normalize` turns records,
    /// oldest first, into messages tagged with their record's index.
    pub async fn fetch(
        q: &ReadQuery,
        mut fetch: impl AsyncFnMut(Option<&str>) -> Result<(Vec<Value>, Option<String>)>,
        normalize: impl Fn(&[Value]) -> Result<Vec<(usize, Message)>>,
    ) -> Result<RemoteHistory> {
        let (before, mut cursor) = match &q.cursor {
            Some(c) => {
                let b: Boundary = serde_json::from_str(c).map_err(|_| {
                    Error::new(ErrorCode::Precondition, format!("invalid cursor {c}"))
                })?;
                (Some(b.item), b.page)
            }
            None => (None, None),
        };
        let mut h = RemoteHistory {
            records: Vec::new(),
            fetched_with: Vec::new(),
            older: None,
            before,
        };
        loop {
            let (page, next) = fetch(cursor.as_deref()).await?;
            h.fetched_with
                .splice(0..0, std::iter::repeat_n(cursor.clone(), page.len()));
            h.records.splice(0..0, page.into_iter().rev());
            let messages = normalize(&h.records)?;
            let cut = cut(
                q,
                h.before.as_deref(),
                messages,
                h.records.len(),
                next.is_some(),
            )?;
            match next {
                Some(c) if cut.messages.len() < q.limit => cursor = Some(c),
                older => {
                    h.older = older;
                    return Ok(h);
                }
            }
        }
    }

    /// The fetched records, oldest first.
    pub fn records(&self) -> &[Value] {
        &self.records
    }

    /// The agent cursor of the next older page, when one exists.
    pub fn older(&self) -> Option<&str> {
        self.older.as_deref()
    }

    /// The page `q` asks for, out of `messages` normalized from `records()`.
    pub fn page(mut self, q: &ReadQuery, messages: Vec<(usize, Message)>) -> Result<ReadPage> {
        let cut = cut(
            q,
            self.before.as_deref(),
            messages,
            self.records.len(),
            self.older.is_some(),
        )?;
        let next_cursor = cut.cursor_item().map(|(i, m)| {
            let b = Boundary {
                item: m.item_id.clone(),
                page: self.fetched_with[*i].clone(),
            };
            serde_json::to_string(&b).expect("serializable")
        });
        Ok(ReadPage {
            messages: Page {
                next_cursor,
                items: cut.messages.into_iter().map(|(_, m)| m).collect(),
            },
            raw: self.records.drain(cut.records).collect(),
        })
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
            cursor: None,
            all: false,
        };
        // The newest two shown messages skip the tool call; the raw span runs to the end.
        let c = cut(&q, None, tagged(HISTORY), 5, false).unwrap();
        assert_eq!(ids(&c), ["2-prompt", "3-final"]);
        assert_eq!((c.records.clone(), c.more), (2..5, true));
        let c = cut(&q, Some("2-prompt"), tagged(HISTORY), 5, false).unwrap();
        assert_eq!(ids(&c), ["1-prompt", "1-final"]);
        assert_eq!((c.records.clone(), c.more), (1..2, false));
        // With every message, a page can end inside a record, which the raw span then
        // includes; a remote history that may hold older records reports more.
        let all = ReadQuery { all: true, ..q };
        let c = cut(&all, Some("3-final"), tagged(HISTORY), 5, true).unwrap();
        assert_eq!(ids(&c), ["2-prompt", "3-other"]);
        assert_eq!((c.records.clone(), c.more), (2..4, true));
        assert!(cut(&all, Some("2-prompt"), tagged(&HISTORY[3..]), 5, false).is_err());
    }

    /// Reading a remote history page by page returns every message once, in order, and a
    /// next page starts at the agent page holding its boundary instead of the newest.
    #[tokio::test]
    async fn remote_history_pages_across_agent_pages() {
        // Six records, one prompt each, served newest first two at a time.
        let agent_page = |cursor: Option<&str>| match cursor {
            None => (vec![5, 4], Some("p2")),
            Some("p2") => (vec![3, 2], Some("p4")),
            Some("p4") => (vec![1, 0], None),
            Some(c) => panic!("unknown agent cursor {c}"),
        };
        let normalize = |records: &[Value]| {
            let messages = records.iter().enumerate().map(|(i, r)| {
                let (_, mut m) = tagged(&[(i, Role::User, Phase::Prompt)]).remove(0);
                m.item_id = r["n"].to_string();
                (i, m)
            });
            Ok(messages.collect())
        };
        let mut q = ReadQuery {
            limit: 3,
            cursor: None,
            all: false,
        };
        let mut pages = Vec::new();
        loop {
            let mut fetched = Vec::new();
            let h = RemoteHistory::fetch(
                &q,
                async |c: Option<&str>| {
                    fetched.push(c.map(String::from));
                    let (records, next) = agent_page(c);
                    let records = records.into_iter().map(|n| serde_json::json!({"n": n}));
                    Ok((records.collect(), next.map(String::from)))
                },
                normalize,
            )
            .await
            .unwrap();
            let messages = normalize(h.records()).unwrap();
            let page = h.page(&q, messages).unwrap();
            let ids: Vec<String> = page
                .messages
                .items
                .iter()
                .map(|m| m.item_id.clone())
                .collect();
            let raw: Vec<String> = page.raw.iter().map(|r| r["n"].to_string()).collect();
            assert_eq!(ids, raw);
            pages.push((ids, fetched));
            match page.messages.next_cursor {
                Some(c) => q.cursor = Some(c),
                None => break,
            }
        }
        let some = |c: &str| Some(c.to_string());
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].0, ["3", "4", "5"]);
        assert_eq!(pages[0].1, [None, some("p2")]);
        assert_eq!(pages[1].0, ["0", "1", "2"]);
        assert_eq!(pages[1].1, [some("p2"), some("p4")]);
    }
}
