//! HTTP + SSE client of the OpenCode background service: registration, authentication,
//! request timeouts, HTTP status to error code, every endpoint the adapter calls with its
//! response decoded, and the `/api/event` stream.

use crate::agents::child::pid_alive;
use crate::model::{AgentError, Error, ErrorCode, Result};
use futures_util::StreamExt;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Rows per history page.
const PAGE: u32 = 50;

#[derive(Deserialize)]
struct Registration {
    url: String,
    pid: u32,
    version: Option<String>,
    password: Option<String>,
}

/// A validated connection to the service.
pub struct Service {
    http: reqwest::Client,
    pub url: String,
    password: Option<String>,
    /// The registration file the url and password came from.
    registration: PathBuf,
    pub pid: u32,
    pub version: String,
}

/// A string field: an empty string, or a value of another type, reads as absent. For
/// `#[serde(default, deserialize_with = "text")]`.
pub fn text<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    })
}

/// The entries of an array field that decode: one odd entry does not fail the whole
/// response, and a value other than an array has none.
fn entries<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    d: D,
) -> std::result::Result<Vec<T>, D::Error> {
    let Value::Array(items) = Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|v| {
            T::deserialize(v)
                .inspect_err(|e| tracing::warn!("opencode service: skipped an entry: {e}"))
                .ok()
        })
        .collect())
}

/// A stored model: one that does not name both its provider and its id reads as none.
fn model<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<ModelRef>, D::Error> {
    Ok(ModelRef::deserialize(Value::deserialize(d)?)
        .ok()
        .filter(|m| !m.provider.is_empty() && !m.id.is_empty()))
}

/// `Model.Ref`: a model as the service stores it on a session; the variant is the effort,
/// which agent-talk sends but never reads back.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider: String,
    pub id: String,
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl ModelRef {
    /// `provider/model`, as `--model` takes it.
    pub fn parse(spec: &str) -> Result<ModelRef> {
        let (provider, id) = spec.split_once('/').ok_or_else(|| {
            Error::new(
                ErrorCode::Precondition,
                format!("--model for OpenCode is <providerID>/<modelID>, got {spec}"),
            )
        })?;
        Ok(ModelRef {
            provider: provider.into(),
            id: id.into(),
            variant: None,
        })
    }
}

/// `Session.Info`, as far as agent-talk reads it. Only the id is required: a listed
/// session without one is skipped, since no command could address it.
#[derive(Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default, deserialize_with = "text")]
    pub title: Option<String>,
    #[serde(rename = "parentID", default, deserialize_with = "text")]
    pub parent_id: Option<String>,
    #[serde(default, deserialize_with = "text")]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "model")]
    pub model: Option<ModelRef>,
    location: Option<Location>,
}

#[derive(Deserialize)]
struct Location {
    #[serde(default, deserialize_with = "text")]
    directory: Option<String>,
}

impl SessionInfo {
    pub fn directory(&self) -> Option<&str> {
        self.location.as_ref()?.directory.as_deref()
    }
}

/// A model of a configured provider, with its variants (efforts). A model without its
/// provider or id is skipped: `--model` could not name it.
#[derive(Deserialize)]
pub struct ApiModel {
    #[serde(rename = "providerID")]
    pub provider: String,
    pub id: String,
    #[serde(default, deserialize_with = "entries")]
    pub variants: Vec<Variant>,
}

#[derive(Deserialize)]
pub struct Variant {
    pub id: String,
}

/// `Permission.Request`, from a `permission.asked` event or `GET …/permission`; `raw` is
/// the request as the service sent it. Only the id is required, since the reply names it;
/// a listed request without one is skipped.
#[derive(Deserialize)]
#[serde(try_from = "Value")]
pub struct PermissionRequest {
    pub id: String,
    /// Empty when the service sent none: such an event matches no session.
    pub session_id: String,
    pub action: Option<String>,
    pub resources: Vec<String>,
    /// The tool call that asks (`call_…`).
    pub call_id: Option<String>,
    pub raw: Value,
}

impl TryFrom<Value> for PermissionRequest {
    type Error = serde_json::Error;

    fn try_from(raw: Value) -> std::result::Result<Self, Self::Error> {
        #[derive(Deserialize)]
        struct Fields {
            id: String,
            #[serde(rename = "sessionID", default)]
            session_id: String,
            #[serde(default, deserialize_with = "text")]
            action: Option<String>,
            #[serde(default, deserialize_with = "entries")]
            resources: Vec<String>,
            source: Option<Source>,
        }
        #[derive(Deserialize)]
        struct Source {
            #[serde(default, deserialize_with = "text")]
            id: Option<String>,
        }
        let f = Fields::deserialize(&raw)?;
        Ok(PermissionRequest {
            id: f.id,
            session_id: f.session_id,
            action: f.action,
            resources: f.resources,
            call_id: f.source.and_then(|s| s.id),
            raw,
        })
    }
}

/// The `/api/event` events agent-talk acts on; every other one is `Other`.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "permission.asked")]
    PermissionAsked { data: PermissionRequest },
    #[serde(rename = "permission.replied")]
    PermissionReplied { data: Replied },
    #[serde(rename = "session.inbox.delivered")]
    InboxDelivered { data: Delivered },
    /// An execution ended (`succeeded`, `failed` or `interrupted`).
    #[serde(
        rename = "session.execution.succeeded",
        alias = "session.execution.failed",
        alias = "session.execution.interrupted"
    )]
    ExecutionEnded { data: SessionRef },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub struct Replied {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
}

#[derive(Deserialize)]
pub struct Delivered {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(rename = "inboxID")]
    pub inbox_id: String,
}

#[derive(Deserialize)]
pub struct SessionRef {
    #[serde(rename = "sessionID")]
    pub session_id: String,
}

/// `{data}`, the envelope of a response with a body.
#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

/// A list response; `cursor.next` names the next page.
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct List<T> {
    #[serde(default, deserialize_with = "entries")]
    data: Vec<T>,
    #[serde(default)]
    cursor: Cursor,
}

#[derive(Deserialize, Default)]
struct Cursor {
    #[serde(default, deserialize_with = "text")]
    next: Option<String>,
}

/// An object of which only the id is read.
#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    pid: u64,
    #[serde(default, deserialize_with = "text")]
    version: Option<String>,
}

/// The query of one page of a list, newest first: the first page by `order=desc`, later
/// ones by the previous page's cursor (the service rejects `order` together with `cursor`,
/// observed on opencode 2.0.22).
fn page_query(limit: u32, cursor: Option<&str>) -> Vec<(&'static str, String)> {
    let mut q = vec![("limit", limit.to_string())];
    match cursor {
        Some(c) => q.push(("cursor", c.into())),
        None => q.push(("order", "desc".into())),
    }
    q
}

fn http_error(status: reqwest::StatusCode, body: &str, registration: &Path) -> Error {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let field = |k: &str| {
        parsed
            .as_ref()
            .and_then(|v| v[k].as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let message = field("message").unwrap_or_else(|| {
        if body.is_empty() {
            status.to_string()
        } else {
            body.chars().take(300).collect()
        }
    });
    let code = match status.as_u16() {
        401 => ErrorCode::NoDaemon,
        400 | 404 | 409 | 422 => ErrorCode::Precondition,
        _ => ErrorCode::Transport,
    };
    let mut e = Error::from_agent(
        code,
        AgentError {
            code: i64::from(status.as_u16()),
            message: match field("_tag") {
                Some(t) => format!("{t}: {message}"),
                None => message,
            },
            data: parsed,
        },
    );
    if status.as_u16() == 401 {
        e.message = format!(
            "the OpenCode service rejected the registered password (stale {}?); restart the service from an OpenCode client",
            registration.display()
        );
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

    /// Send a request; a status other than success is an error.
    async fn respond(&self, b: reqwest::RequestBuilder) -> Result<reqwest::Response> {
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
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp
            .text()
            .await
            .map_err(|e| Error::transport("opencode service", e))?;
        Err(http_error(status, &body, &self.registration))
    }

    /// Send a request and decode its JSON body (an empty body reads as `null`).
    async fn call<T: DeserializeOwned>(&self, b: reqwest::RequestBuilder) -> Result<T> {
        let body = self
            .respond(b)
            .await?
            .text()
            .await
            .map_err(|e| Error::transport("opencode service", e))?;
        let body = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(body)
            .map_err(|e| Error::transport("opencode service: unexpected response", e))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        self.call(self.request(reqwest::Method::GET, path).query(query))
            .await
    }

    /// A request whose response body agent-talk does not read.
    async fn send(&self, b: reqwest::RequestBuilder) -> Result<()> {
        self.call::<IgnoredAny>(b).await?;
        Ok(())
    }

    async fn post(&self, path: &str, body: &Value) -> Result<()> {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    /// One page of a session's history, newest first (`page_query`): the raw rows and the
    /// cursor of the next older page. The page after the last one is empty.
    pub async fn history(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let page: List<Value> = self
            .get(
                &format!("/api/session/{session_id}/message"),
                &page_query(PAGE, cursor),
            )
            .await?;
        Ok((page.data, page.cursor.next))
    }

    /// The id of the newest user message behind a history page's `cursor` toward older rows
    /// (a cursor is an anchor row plus a direction; `type` filters any cursor read; observed
    /// on opencode 2.0.22).
    pub async fn user_before(&self, session_id: &str, cursor: &str) -> Result<Option<String>> {
        let page: List<Id> = self
            .get(
                &format!("/api/session/{session_id}/message"),
                &[
                    ("cursor", cursor.into()),
                    ("type", "user".into()),
                    ("limit", "1".into()),
                ],
            )
            .await?;
        Ok(page.data.into_iter().next().map(|u| u.id))
    }

    /// The ids of the messages waiting in a session's inbox.
    pub async fn inbox(&self, session_id: &str) -> Result<Vec<String>> {
        let inbox: List<Id> = self
            .get(&format!("/api/session/{session_id}/inbox"), &[])
            .await?;
        Ok(inbox.data.into_iter().map(|i| i.id).collect())
    }

    /// The sessions with a running execution.
    pub async fn active(&self) -> Result<HashSet<String>> {
        let active: Data<Option<HashMap<String, IgnoredAny>>> =
            self.get("/api/session/active", &[]).await?;
        Ok(active.data.unwrap_or_default().into_keys().collect())
    }

    /// Requests already pending are not replayed on a new subscription (DESIGN.md §6.3).
    pub async fn permissions(&self, session_id: &str) -> Result<Vec<PermissionRequest>> {
        let list: List<PermissionRequest> = self
            .get(&format!("/api/session/{session_id}/permission"), &[])
            .await?;
        Ok(list.data)
    }

    /// Answer a permission request with `reject` and a message for the model.
    pub async fn reject(&self, session_id: &str, request_id: &str, message: &str) -> Result<()> {
        self.post(
            &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            &json!({"decision": "reject", "message": message}),
        )
        .await
    }

    pub async fn prompt(&self, session_id: &str, body: &Value) -> Result<()> {
        self.post(&format!("/api/session/{session_id}/prompt"), body)
            .await
    }

    /// One page of sessions, newest first (`page_query`), and the cursor of the next one.
    pub async fn sessions(
        &self,
        limit: u32,
        cursor: Option<&str>,
        directory: Option<&str>,
        top_level: bool,
    ) -> Result<(Vec<SessionInfo>, Option<String>)> {
        let mut q = page_query(limit, cursor);
        if let Some(d) = directory {
            q.push(("directory", d.into()));
        }
        if top_level {
            q.push(("parentID", "null".into()));
        }
        let page: List<SessionInfo> = self.get("/api/session", &q).await?;
        Ok((page.data, page.cursor.next))
    }

    pub async fn session(&self, session_id: &str) -> Result<SessionInfo> {
        let s: Data<SessionInfo> = self.get(&format!("/api/session/{session_id}"), &[]).await?;
        Ok(s.data)
    }

    /// `session.create`; returns the new session's id.
    pub async fn create_session(&self, body: &Value) -> Result<String> {
        let created: Data<Id> = self
            .call(
                self.request(reqwest::Method::POST, "/api/session")
                    .json(body),
            )
            .await?;
        Ok(created.data.id)
    }

    pub async fn set_model(&self, session_id: &str, model: &ModelRef) -> Result<()> {
        self.post(
            &format!("/api/session/{session_id}/model"),
            &json!({ "model": model }),
        )
        .await
    }

    pub async fn set_permissions(&self, session_id: &str, rules: &Value) -> Result<()> {
        self.send(
            self.request(
                reqwest::Method::PATCH,
                &format!("/api/session/{session_id}"),
            )
            .json(&json!({ "permissions": rules })),
        )
        .await
    }

    /// Every model of every configured provider.
    pub async fn models(&self) -> Result<Vec<ApiModel>> {
        let models: List<ApiModel> = self.get("/api/model", &[]).await?;
        Ok(models.data)
    }

    /// The user's default model for `directory`.
    pub async fn default_model(&self, directory: &str) -> Result<Option<ModelRef>> {
        #[derive(Deserialize)]
        struct DefaultModel {
            #[serde(default, deserialize_with = "model")]
            data: Option<ModelRef>,
        }
        let default: DefaultModel = self
            .get(
                "/api/model/default",
                &[("location[directory]", directory.into())],
            )
            .await?;
        Ok(default.data)
    }

    /// The rules of the agent a new session in `directory` runs: the default agent, which
    /// the service lists first (read as `Value`, so that no entry is skipped before it).
    pub async fn default_agent_rules(&self, directory: &str) -> Result<Option<Vec<Value>>> {
        let agents: List<Value> = self
            .get("/api/agent", &[("location[directory]", directory.into())])
            .await?;
        Ok(agents
            .data
            .into_iter()
            .next()
            .and_then(|mut a| match a["permissions"].take() {
                Value::Array(rules) => Some(rules),
                _ => None,
            }))
    }

    /// `integration.list`, which returns once the location's plugins are active, and with
    /// them its configuration.
    pub async fn load_location(&self, directory: &str) -> Result<()> {
        self.send(
            self.request(reqwest::Method::GET, "/api/integration")
                .query(&[("location[directory]", directory)]),
        )
        .await
    }

    pub async fn events(&self) -> Result<Events> {
        let resp = self
            .respond(
                self.request(reqwest::Method::GET, "/api/event")
                    .timeout(Duration::from_secs(60 * 60 * 24)),
            )
            .await?;
        Ok(Events {
            body: Box::pin(resp.bytes_stream()),
            buf: Vec::new(),
        })
    }
}

/// `GET /api/event`: one JSON object per `data:` line, no ids, no replay.
pub struct Events {
    body: Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
}

impl Events {
    /// Next event, or `None` when the service closed the stream.
    pub async fn next(&mut self) -> Result<Option<Event>> {
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
                match serde_json::from_str::<Value>(&data.join("\n")) {
                    // An event of an unexpected shape is one agent-talk does not act on.
                    Ok(v) => return Ok(Some(Event::deserialize(&v).unwrap_or(Event::Other))),
                    Err(e) => {
                        tracing::warn!("opencode events: undecodable frame: {e}");
                        continue;
                    }
                }
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    return Err(Error::transport("opencode events", e));
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
        registration: registration.to_path_buf(),
        pid: reg.pid,
        version: reg.version.unwrap_or_default(),
    };
    let info: Info = svc.get("/api/info", &[]).await.map_err(|mut e| {
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
    if info.pid != u64::from(reg.pid) {
        return Err(Error::new(
            ErrorCode::NoDaemon,
            format!(
                "registration names pid {} but {} is served by pid {}; stale registration",
                reg.pid, svc.url, info.pid
            ),
        ));
    }
    Ok(Service {
        version: info.version.unwrap_or(svc.version),
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
            Ok(bytes::Bytes::from_static(b"beat\n\ndata: {\"id\":\"evt_2\",\"type\":\"session.execution.interrupted\",\"data\":{\"sessionID\":\"ses_x\",\"reason\":\"user\"}}\n\n")),
        ];
        let mut ev = Events {
            body: Box::pin(futures_util::stream::iter(chunks)),
            buf: Vec::new(),
        };
        assert!(matches!(ev.next().await.unwrap(), Some(Event::Other)));
        match ev.next().await.unwrap() {
            Some(Event::ExecutionEnded { data }) => assert_eq!(data.session_id, "ses_x"),
            _ => panic!("expected an execution end"),
        }
        assert!(ev.next().await.unwrap().is_none());
    }

    /// Shapes observed on opencode 2.0.22.
    #[test]
    fn permission_requests_keep_their_raw_form() {
        let raw = json!({"id":"evt_1","type":"permission.asked","location":{"directory":"/w"},
            "data":{"id":"per_1","sessionID":"ses_x","action":"edit","resources":["a.txt"],"save":["*"],
                "source":{"type":"tool","messageID":"msg_a","id":"call_1"}}});
        let Event::PermissionAsked { data: req } = Event::deserialize(&raw).unwrap() else {
            panic!("expected a permission request");
        };
        assert_eq!(
            (req.id.as_str(), req.session_id.as_str()),
            ("per_1", "ses_x")
        );
        assert_eq!(req.call_id.as_deref(), Some("call_1"));
        assert_eq!(req.raw, raw["data"]);
    }

    /// An odd entry or field does not fail a listing; an empty string is absent.
    #[test]
    fn lists_tolerate_odd_entries() {
        let sessions: List<SessionInfo> = serde_json::from_value(json!({"data": [
            {"id": "ses_a", "title": "", "model": {"id": "m"}, "location": {"directory": ""}},
            {"title": "no id"},
            {"id": "ses_b", "model": {"providerID": "p", "id": "m", "variant": 3}},
        ], "cursor": {"next": ""}}))
        .unwrap();
        let [a, b] = &sessions.data[..] else {
            panic!("expected two sessions");
        };
        assert!(a.title.is_none() && a.model.is_none() && a.directory().is_none());
        assert_eq!(b.model.as_ref().map(|m| m.id.as_str()), Some("m"));
        assert!(sessions.cursor.next.is_none());

        let requests: List<PermissionRequest> = serde_json::from_value(json!({"data": [
            {"id": "per_1", "action": "", "resources": ["a.txt", {"path": "b"}]},
            {"action": "edit"},
        ]}))
        .unwrap();
        let [req] = &requests.data[..] else {
            panic!("expected one request");
        };
        assert_eq!(
            (req.action.as_deref(), &req.resources[..]),
            (None, &["a.txt".to_string()][..])
        );
    }
}
