//! The receipt and approval lifecycle, and the bounds of an observation. These are
//! agent-talk's semantics (DESIGN.md §4), not an agent's, so they are implemented once here;
//! the adapters supply the agent plumbing.

use crate::model::{Approval, Error, ErrorCode, Outcome, Receipt, ReceiptState, Result, Turn};
use crate::store::Store;
use serde_json::Value;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

/// How agent-talk treats approval requests that reach it while it observes a turn
/// (DESIGN.md §4). Every subscribed client receives them, and any answer resolves them for
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Never answer; record the request and report it as pending. Sessions agent-talk did
    /// not start: the user answers in their own client.
    Observe,
    /// Decline, never accept: Codex answers `decline`, OpenCode `reject` with a message,
    /// Claude gets `--permission-prompts none`, Grok `reject_once`; Antigravity and pi have
    /// nothing to answer. Sessions agent-talk started, where nobody else would answer.
    Deny,
}

/// Deny on sessions agent-talk started, observe on the others.
pub fn approval_policy(store: &Store, handle: &str) -> Result<ApprovalPolicy> {
    Ok(if store.is_owned(handle)? {
        ApprovalPolicy::Deny
    } else {
        ApprovalPolicy::Observe
    })
}

/// The submission failed. An agent response is a definitive refusal: `rejected`. A lost
/// outcome (connection lost, HTTP 5xx, undecodable reply, no response in time, Ctrl-C)
/// is `unknown`, to be recovered with `wait --receipt`.
pub fn reject(store: &Store, receipt_id: &str, e: Error) -> Error {
    let state = if e.code.is_unknown_outcome() {
        ReceiptState::Unknown
    } else {
        ReceiptState::Rejected
    };
    if let Err(store_err) = store.settle_unaccepted(receipt_id, state, e.agent_error.as_deref()) {
        tracing::warn!("{store_err}");
    }
    e
}

pub fn record(store: &Store, a: &Approval) {
    if let Err(e) = store.insert_approval(a) {
        tracing::warn!("{e}");
    }
}

/// Another client answered the request: pending approvals with this id are resolved.
pub fn resolve(store: &Store, approvals: &mut [Approval], request_id: &Value) {
    for a in approvals
        .iter_mut()
        .filter(|a| a.request_id == *request_id && a.outcome == "pending")
    {
        a.outcome = "resolved";
        record(store, a);
    }
}

/// The receipt a `wait --receipt` targets: known, on this handle, not rejected.
pub fn wait_receipt(store: &Store, handle: &str, receipt_id: &str) -> Result<Receipt> {
    let rec = store.receipt(receipt_id)?.ok_or_else(|| {
        Error::new(
            ErrorCode::Precondition,
            format!("unknown receipt {receipt_id}"),
        )
    })?;
    if rec.handle != handle {
        return Err(Error::new(
            ErrorCode::Precondition,
            format!("receipt {receipt_id} belongs to {}", rec.handle),
        ));
    }
    if rec.state == ReceiptState::Rejected {
        let mut e = Error::new(ErrorCode::Precondition, "the intent was rejected");
        e.agent_error = rec.agent_error.clone().map(Box::new);
        e.receipt = Some(Box::new(rec));
        return Err(e);
    }
    Ok(rec)
}

/// Settle an observed turn. The receipt is reloaded; when the outcome is unknown
/// (deadline, Ctrl-C, transport loss) a still-pending receipt becomes `unknown` and the
/// error reports `progress` (`waiting` while an approval is unanswered). Approvals seen
/// during the observation travel with the outcome either way.
pub fn settle(
    store: &Store,
    handle: String,
    res: Result<Option<Turn>>,
    receipt_id: Option<&str>,
    approvals: Vec<Approval>,
    progress: &'static str,
) -> Result<Outcome> {
    let unknown_outcome = matches!(&res, Err(e) if e.code.is_unknown_outcome());
    if unknown_outcome && let Some(r) = receipt_id {
        store.settle_unaccepted(r, ReceiptState::Unknown, None)?;
    }
    let receipt = match receipt_id {
        Some(r) => store.receipt(r)?,
        None => None,
    };
    match res {
        Ok(turn) => Ok(Outcome {
            handle,
            receipt,
            turn,
            approvals,
            from: None,
        }),
        Err(mut e) => {
            e.receipt = e.receipt.or(receipt.map(Box::new));
            if unknown_outcome {
                e.state = Some(if approvals.iter().any(|a| a.outcome == "pending") {
                    "waiting"
                } else {
                    progress
                });
            }
            e.approvals = approvals;
            Err(e)
        }
    }
}

/// Whether Ctrl-C and SIGTERM end observation. Once tokio listens for a signal, its default
/// action never returns for the life of the process, so only the CLI, which exits after one
/// operation, listens; `agent-talk mcp` keeps the default actions (DESIGN.md §4).
static INTERRUPTIBLE: AtomicBool = AtomicBool::new(false);

/// Let Ctrl-C and SIGTERM end observation in this process.
pub fn interrupt_on_signals() {
    INTERRUPTIBLE.store(true, Ordering::Relaxed);
}

/// Resolves on Ctrl-C or SIGTERM (Claude Code's Bash tool sends SIGTERM when it stops a
/// background command); never when signals are not enabled for this process.
async fn interrupted() {
    if !INTERRUPTIBLE.load(Ordering::Relaxed) {
        return std::future::pending().await;
    }
    let term = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}

/// Run `fut` until it finishes, the deadline passes, or Ctrl-C or SIGTERM arrives. Without a
/// deadline only a signal ends it early.
pub async fn bounded<T>(
    deadline: Option<Instant>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    let until = async {
        match deadline {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = fut => r,
        _ = until => Err(Error::new(
            ErrorCode::Timeout,
            "deadline passed before the turn completed; outcome unknown, receipt retained",
        )),
        _ = interrupted() => Err(Error::new(
            ErrorCode::Interrupted,
            "interrupted; outcome unknown, receipt retained",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Outgoing;
    use crate::model::{AgentError, Caller};
    use crate::store::NewIntent;

    fn pending(store: &Store, id: &str) {
        let message = Outgoing {
            text: "x",
            delivered: "x",
            from: &Caller::UNKNOWN,
            depth: 0,
        };
        store
            .insert_intent(&NewIntent {
                receipt_id: id,
                handle: "codex:t",
                client_msg_id: &format!("c-{id}"),
                message: &message,
            })
            .unwrap();
    }

    fn state(store: &Store, id: &str) -> ReceiptState {
        store.receipt(id).unwrap().unwrap().state
    }

    /// A lost outcome keeps the intent recoverable, only an agent refusal is final, and
    /// neither overwrites an acceptance another command already recorded.
    #[test]
    fn submission_failures_settle_by_certainty() {
        let s = Store::memory();
        pending(&s, "lost");
        reject(&s, "lost", Error::new(ErrorCode::Timeout, "no response"));
        assert_eq!(state(&s, "lost"), ReceiptState::Unknown);
        // Later proof of delivery (history, queue, inbox, run log).
        s.accept("lost", None, Some("turn-1"), None).unwrap();
        assert_eq!(state(&s, "lost"), ReceiptState::Accepted);

        pending(&s, "refused");
        let agent_error = AgentError {
            code: -32600,
            message: "stale turn".into(),
            data: None,
        };
        reject(
            &s,
            "refused",
            Error::from_agent(ErrorCode::Precondition, agent_error),
        );
        assert_eq!(state(&s, "refused"), ReceiptState::Rejected);

        pending(&s, "raced");
        s.accept("raced", Some("q1"), None, None).unwrap();
        s.settle_unaccepted("raced", ReceiptState::Unknown, None)
            .unwrap();
        assert_eq!(state(&s, "raced"), ReceiptState::Accepted);
    }
}
