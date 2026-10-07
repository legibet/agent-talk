//! HTTP + SSE client of the OpenCode background service: registration, authentication,
//! request timeouts, HTTP status to error code, and the `/api/event` frames.

use super::{take_data, text};
use crate::agents::pid_alive;
use crate::model::{AgentError, Error, ErrorCode, Result};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PAGE: u32 = 50;

#[derive(Deserialize)]
struct Registration {
    url: String,
    pid: u32,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// A validated connection to the service.
pub struct Service {
    http: reqwest::Client,
    pub url: String,
    password: Option<String>,
    pub pid: u32,
    pub version: String,
}

/// `GET /api/event`: one JSON object per `data:` line, no ids, no replay.
pub struct Events {
    body: Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
}

fn http_error(status: reqwest::StatusCode, body: &str) -> Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| text(&v["message"]))
        .unwrap_or_else(|| {
            if body.is_empty() {
                status.to_string()
            } else {
                body.chars().take(300).collect()
            }
        });
    let tag = parsed.as_ref().and_then(|v| text(&v["_tag"]));
    let code = match status.as_u16() {
        401 => ErrorCode::NoDaemon,
        400 | 404 | 409 | 422 => ErrorCode::Precondition,
        _ => ErrorCode::Transport,
    };
    let mut e = Error::from_agent(
        code,
        AgentError {
            code: i64::from(status.as_u16()),
            message: match &tag {
                Some(t) => format!("{t}: {message}"),
                None => message,
            },
            data: parsed,
        },
    );
    if status.as_u16() == 401 {
        e.message = "the OpenCode service rejected the registered password (stale ~/.local/state/opencode/service.json?); restart the service from an OpenCode client".into();
    }
    e
}

impl Service {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let b = self
            .http
            .request(method, format!("{}{path}", self.url))
            .timeout(REQUEST_TIMEOUT);
        match &self.password {
            Some(p) => b.basic_auth("opencode", Some(p)),
            None => b,
        }
    }

    async fn send(&self, b: reqwest::RequestBuilder) -> Result<Value> {
        let resp = b.send().await.map_err(|e| {
            // reqwest's Display stops at "error sending request"; the io cause below it
            // (connection refused, operation not permitted) is what tells the failures apart.
            let mut message = format!("opencode service: {e}");
            let mut cause = std::error::Error::source(&e);
            while let Some(c) = cause {
                message.push_str(&format!(": {c}"));
                cause = c.source();
            }
            Error::new(ErrorCode::Transport, message)
        })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::new(ErrorCode::Transport, format!("opencode service: {e}")))?;
        if !status.is_success() {
            return Err(http_error(status, &body));
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).map_err(|e| {
            Error::new(
                ErrorCode::Transport,
                format!("opencode service: invalid JSON in response: {e}"),
            )
        })
    }

    /// One page of a session's history, newest first: the first page by `order=desc`,
    /// later ones by the previous page's cursor (the service rejects `order` together
    /// with `cursor`, observed on opencode 2.0.22). Returns the rows and the next cursor; the
    /// page after the last one is empty.
    pub async fn history(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let mut q = vec![("limit", PAGE.to_string())];
        match cursor {
            Some(c) => q.push(("cursor", c.into())),
            None => q.push(("order", "desc".into())),
        }
        let mut page = self
            .get(&format!("/api/session/{session_id}/message"), &q)
            .await?;
        let rows = take_data(&mut page);
        Ok((rows, text(&page["cursor"]["next"])))
    }

    /// Whether the session has a running execution.
    pub async fn active(&self, session_id: &str) -> Result<bool> {
        Ok(self.get("/api/session/active", &[]).await?["data"]
            .get(session_id)
            .is_some())
    }

    /// Requests already pending are not replayed on a new subscription (DESIGN.md §6.3).
    pub async fn permissions(&self, session_id: &str) -> Result<Vec<Value>> {
        let mut list = self
            .get(&format!("/api/session/{session_id}/permission"), &[])
            .await?;
        Ok(take_data(&mut list))
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(self.request(reqwest::Method::GET, path).query(query))
            .await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.request(reqwest::Method::PATCH, path).json(body))
            .await
    }

    pub async fn events(&self) -> Result<Events> {
        let resp = self
            .request(reqwest::Method::GET, "/api/event")
            .timeout(Duration::from_secs(60 * 60 * 24))
            .send()
            .await
            .map_err(|e| Error::new(ErrorCode::Transport, format!("opencode events: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(http_error(status, &body));
        }
        Ok(Events {
            body: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
        })
    }
}

impl Events {
    /// Next event object, or `None` when the service closed the stream.
    pub async fn next(&mut self) -> Result<Option<Value>> {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
                let frame = String::from_utf8_lossy(&frame);
                let data: Vec<&str> = frame
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if data.is_empty() {
                    continue; // heartbeat comment
                }
                let joined = data.join("\n");
                match serde_json::from_str::<Value>(&joined) {
                    Ok(v) => return Ok(Some(v)),
                    Err(e) => {
                        tracing::warn!("opencode events: undecodable frame: {e}");
                        continue;
                    }
                }
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    return Err(Error::new(
                        ErrorCode::Transport,
                        format!("opencode events: {e}"),
                    ));
                }
                None => return Ok(None),
            }
        }
    }
}

/// Read the registration and check it against the live service (`/api/info` pid).
pub async fn connect(registration: &Path) -> Result<Service> {
    let raw = tokio::fs::read_to_string(registration).await.map_err(|e| {
        Error::new(
            ErrorCode::NoDaemon,
            format!(
                "OpenCode background service is not registered ({}: {e}); start it with `opencode service start` or by opening an OpenCode client. agent-talk never starts it",
                registration.display()
            ),
        )
    })?;
    let reg: Registration = serde_json::from_str(&raw).map_err(|e| {
        Error::new(
            ErrorCode::NoDaemon,
            format!("unreadable registration {}: {e}", registration.display()),
        )
    })?;
    let svc = Service {
        http: reqwest::Client::new(),
        url: reg.url.trim_end_matches('/').to_string(),
        password: reg.password,
        pid: reg.pid,
        version: reg.version.unwrap_or_default(),
    };
    let info = svc.get("/api/info", &[]).await.map_err(|mut e| {
        if e.code == ErrorCode::Transport {
            if pid_alive(reg.pid) {
                e.message = format!(
                    "cannot reach the OpenCode background service at {} although its pid {} is alive: {}. A sandbox that blocks local network access fails this way; agent-talk needs to reach the service's localhost port",
                    svc.url, reg.pid, e.message
                );
            } else {
                e.code = ErrorCode::NoDaemon;
                e.message = format!(
                    "OpenCode background service registered at {} (pid {}) is gone; stale registration {}. It restarts when an OpenCode client starts; agent-talk never starts it",
                    svc.url,
                    reg.pid,
                    registration.display()
                );
            }
        }
        e
    })?;
    let live_pid = info["pid"].as_u64().unwrap_or_default();
    if live_pid != u64::from(reg.pid) {
        return Err(Error::new(
            ErrorCode::NoDaemon,
            format!(
                "registration names pid {} but {} is served by pid {live_pid}; stale registration",
                reg.pid, svc.url
            ),
        ));
    }
    Ok(Service {
        version: text(&info["version"]).unwrap_or(svc.version),
        ..svc
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sse_frames_are_split_on_blank_lines() {
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![
            Ok(bytes::Bytes::from_static(b"data: {\"id\":\"evt_1\",\"type\":\"server.connected\",\"data\":{}}\n\n: heart")),
            Ok(bytes::Bytes::from_static(b"beat\n\ndata: {\"id\":\"evt_2\",\"type\":\"session.execution.succeeded\",\"data\":{\"sessionID\":\"ses_x\"}}\n\n")),
        ];
        let mut ev = Events {
            body: Box::pin(futures_util::stream::iter(chunks)),
            buf: Vec::new(),
        };
        assert_eq!(
            ev.next().await.unwrap().unwrap()["type"],
            "server.connected"
        );
        let second = ev.next().await.unwrap().unwrap();
        assert_eq!(second["type"], "session.execution.succeeded");
        assert_eq!(second["data"]["sessionID"], "ses_x");
        assert!(ev.next().await.unwrap().is_none());
    }
}
