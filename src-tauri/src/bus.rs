use std::fs;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tungstenite::{accept, Message};

use crate::approvals::{ApprovalDecision, ApprovalRegistry};
use crate::focus;
use crate::notification_filter::NotificationFilter;

const SESSION_FILE: &str = "bus-session.json";
const MAX_EVENT_BYTES: usize = 64 * 1024;
const APPROVAL_WAIT_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

impl Event {
    fn is_valid(&self) -> bool {
        let source_kind_status_valid = match (self.source.as_str(), self.kind.as_str()) {
            ("ai", "task") => {
                self.agent.as_deref().is_none_or(|agent| {
                    matches!(
                        agent,
                        "codex" | "antigravity-app" | "antigravity-cli" | "antigravity-ide" | "omp"
                    )
                }) && self
                    .session_id
                    .as_ref()
                    .is_none_or(|value| !value.trim().is_empty())
                    && matches!(
                        self.status.as_deref(),
                        Some("running" | "done" | "error" | "needs_input")
                    )
            }
            ("chat", "message") => self.status.as_deref() == Some("unread"),
            ("system", "metric") => matches!(self.status.as_deref(), None | Some("warn" | "crit")),
            _ => false,
        };

        source_kind_status_valid
            && is_uuid(&self.id)
            && !self.title.trim().is_empty()
            && self
                .progress
                .is_none_or(|value| value.is_finite() && (0.0..=1.0).contains(&value))
            && (self.progress.is_none() || self.kind == "task")
            && self.started_at.as_deref().is_none_or(is_iso8601)
            && self
                .cwd
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty())
            && self.usage.as_ref().is_none_or(|value| value.is_object())
            && self.actions.as_ref().is_none_or(|actions| {
                !actions.is_empty()
                    && actions
                        .iter()
                        .all(|action| matches!(action.as_str(), "approve" | "reject"))
            })
            && (self.actions.is_none() || self.source == "ai")
            && (self.action_nonce.is_none() || self.actions.is_some())
            && self.meta.as_ref().is_none_or(serde_json::Value::is_object)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    url: String,
    token: String,
}

pub fn start(
    app: AppHandle,
    session_dir: &Path,
    approvals: Arc<ApprovalRegistry>,
    codex_enabled: Arc<AtomicBool>,
) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let token = random_token()?;
    write_session_file(session_dir, address, &token)?;
    let notification_filter = Arc::new(Mutex::new(NotificationFilter::load(
        &session_dir.join("notification-filter.json"),
    )));

    thread::Builder::new()
        .name("dot-notifier-event-bus".into())
        .spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let app = app.clone();
                    let token = token.clone();
                    let filter = notification_filter.clone();
                    let approvals = approvals.clone();
                    let codex_enabled = codex_enabled.clone();
                    let _ = thread::Builder::new()
                        .name("dot-notifier-bus-client".into())
                        .spawn(move || {
                            handle_connection(
                                &app,
                                stream,
                                &token,
                                &filter,
                                &approvals,
                                &codex_enabled,
                            )
                        });
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    eprintln!("Event bus accept failed: {error}");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn write_session_file(directory: &Path, address: SocketAddr, token: &str) -> Result<(), String> {
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let session = SessionInfo {
        url: format!("ws://{address}/events"),
        token: token.to_owned(),
    };
    let serialized = serde_json::to_vec(&session).map_err(|error| error.to_string())?;
    let temporary_path = directory.join(format!("{SESSION_FILE}.tmp"));
    fs::write(&temporary_path, serialized).map_err(|error| error.to_string())?;
    fs::rename(temporary_path, directory.join(SESSION_FILE)).map_err(|error| error.to_string())
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn is_integration_enabled(agent: Option<&str>, codex_enabled: bool) -> bool {
    agent != Some("codex") || codex_enabled
}

fn handle_connection(
    app: &AppHandle,
    stream: TcpStream,
    token: &str,
    notification_filter: &Mutex<NotificationFilter>,
    approvals: &ApprovalRegistry,
    codex_enabled: &AtomicBool,
) {
    if !is_authorized_upgrade(&stream, token) {
        use std::io::Write;

        let mut stream = stream;
        let _ = stream.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }

    let mut socket = match accept(stream) {
        Ok(socket) => socket,
        Err(_) => return,
    };

    let message = match socket.read() {
        Ok(Message::Text(text)) if text.len() <= MAX_EVENT_BYTES => text,
        Ok(_) => {
            eprintln!("Dropped invalid event bus frame");
            return;
        }
        Err(_) => return,
    };
    let mut event = match parse_event(&message) {
        Some(event) => event,
        None => {
            eprintln!("Dropped malformed event bus payload");
            return;
        }
    };
    if !is_integration_enabled(
        event.agent.as_deref(),
        codex_enabled.load(Ordering::Acquire),
    ) {
        return;
    }
    event.actions = None;
    event.action_nonce = None;
    event.pid = event.pid.map(focus::focus_target_pid);
    let is_permission_request = event.agent.as_deref() == Some("codex")
        && event.status.as_deref() == Some("needs_input")
        && serde_json::from_str::<serde_json::Value>(&message)
            .ok()
            .and_then(|payload| {
                payload
                    .get("hook_event_name")
                    .and_then(|name| name.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("PermissionRequest");
    let Ok(mut filter) = notification_filter.lock() else {
        return;
    };
    if !filter.should_forward(&event) {
        return;
    }
    drop(filter);

    let mut socket = socket;
    let mut pending = None;
    if is_permission_request {
        match approvals.register(&event.id) {
            Ok((nonce, receiver)) => {
                event.actions = Some(vec!["approve".into(), "reject".into()]);
                event.action_nonce = Some(nonce.clone());
                pending = Some((nonce, receiver));
            }
            Err(error) => {
                eprintln!("Could not register permission request: {error}");
                return;
            }
        }
    }
    let mut cleanup_event = event.clone();
    let event_id = event.id.clone();
    if let Err(error) = app.emit("bus-event", event) {
        eprintln!("Could not deliver event to UI: {error}");
        if let Some((nonce, _)) = pending.as_ref() {
            approvals.cancel(&event_id, nonce);
        }
        return;
    }
    if let Some((nonce, receiver)) = pending {
        let expires_at = Instant::now() + APPROVAL_WAIT_LIMIT;
        loop {
            match receiver.try_recv() {
                Ok(decision) => {
                    let response = match decision {
                        ApprovalDecision::Allow => serde_json::json!({
                            "hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}
                        }),
                        ApprovalDecision::Deny => serde_json::json!({
                            "hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "deny"}}
                        }),
                    };
                    let _ = socket.send(Message::Text(response.to_string().into()));
                    let _ = socket.close(None);
                    cleanup_event.actions = None;
                    cleanup_event.action_nonce = None;
                    cleanup_event.status = Some("running".into());
                    let _ = app.emit("bus-event", cleanup_event);
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match socket.read() {
                Ok(Message::Close(_)) => break,
                Ok(_) => break,
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
            if Instant::now() >= expires_at {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        approvals.cancel(&event_id, &nonce);
        cleanup_event.actions = None;
        cleanup_event.action_nonce = None;
        let _ = app.emit("bus-event", cleanup_event);
        let _ = socket.close(None);
        return;
    }
    let _ = socket.close(None);
}

fn is_authorized_upgrade(stream: &TcpStream, token: &str) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.peek(&mut buffer) {
            Ok(0) => return false,
            Ok(length) => {
                let request = &buffer[..length];
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let Ok(headers) = std::str::from_utf8(&request[..end]) else {
                        return false;
                    };
                    let mut lines = headers.split("\r\n");
                    if lines.next() != Some("GET /events HTTP/1.1") {
                        return false;
                    }
                    let expected = format!("Bearer {token}");
                    let mut authorization = lines.filter_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then_some(value.trim())
                    });
                    return authorization.next() == Some(expected.as_str())
                        && authorization.next().is_none();
                }
                if length == buffer.len() {
                    return false;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn parse_event(payload: &str) -> Option<Event> {
    if let Ok(event) = serde_json::from_str::<Event>(payload) {
        if event.is_valid() {
            return Some(event);
        }
    }

    let notification: serde_json::Value = serde_json::from_str(payload).ok()?;
    if notification
        .get("hook_event_name")
        .and_then(|value| value.as_str())
        == Some("PermissionRequest")
    {
        let session_id = notification.get("session_id")?.as_str()?;
        let event = Event {
            id: session_id.to_ascii_lowercase(),
            source: "ai".into(),
            agent: Some("codex".into()),
            session_id: Some(session_id.to_ascii_lowercase()),
            kind: "task".into(),
            status: Some("needs_input".into()),
            title: "Codex is waiting for permission. Respond in Codex.".into(),
            body: None,
            progress: None,
            started_at: None,
            cwd: None,
            pid: notification
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok()),
            usage: None,
            actions: None,
            action_nonce: None,
            meta: None,
        };
        return event.is_valid().then_some(event);
    }

    let thread_id = notification
        .get("thread-id")
        .or_else(|| notification.get("thread_id"))?
        .as_str()?;
    let status = match notification.get("type")?.as_str()? {
        "agent-turn-complete" => "done",
        _ => return None,
    };
    let event = Event {
        id: thread_id.to_ascii_lowercase(),
        source: "ai".into(),
        agent: Some("codex".into()),
        session_id: Some(thread_id.to_owned()),
        kind: "task".into(),
        status: Some(status.into()),
        title: "Codex task completed".into(),
        body: None,
        progress: None,
        started_at: None,
        cwd: None,
        pid: notification
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        usage: None,
        actions: None,
        action_nonce: None,
        meta: None,
    };
    event.is_valid().then_some(event)
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn is_iso8601(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return false;
    }

    for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
        if !bytes[index].is_ascii_digit() {
            return false;
        }
    }

    let timezone_start = if matches!(bytes.last(), Some(b'Z' | b'z')) {
        bytes.len() - 1
    } else if let Some(index) = bytes[19..]
        .iter()
        .position(|byte| matches!(byte, b'+' | b'-'))
    {
        index + 19
    } else {
        return false;
    };
    let seconds = &bytes[17..timezone_start];
    if seconds.len() != 2
        && (seconds.len() < 4 || seconds[2] != b'.' || !seconds[3..].iter().all(u8::is_ascii_digit))
    {
        return false;
    }

    if timezone_start < bytes.len() - 1 {
        let timezone = &bytes[timezone_start..];
        if timezone.len() != 6
            || !matches!(timezone[0], b'+' | b'-')
            || timezone[3] != b':'
            || ![timezone[1], timezone[2], timezone[4], timezone[5]]
                .iter()
                .all(u8::is_ascii_digit)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{is_integration_enabled, is_uuid, parse_event};

    #[test]
    fn codex_events_follow_the_integration_switch() {
        assert!(is_integration_enabled(Some("codex"), true));
        assert!(!is_integration_enabled(Some("codex"), false));
        assert!(is_integration_enabled(Some("antigravity-app"), false));
        assert!(is_integration_enabled(None, false));
    }

    #[test]
    fn accepts_valid_payload_and_ignores_unknown_fields() {
        let payloads: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/codex-notify.json"))
                .expect("fixture JSON is valid");
        let valid = parse_event(&payloads["valid"].to_string()).expect("valid notify payload");
        let with_extra = parse_event(&payloads["unknownFields"].to_string())
            .expect("unknown fields are ignored");

        assert!(valid.is_valid());
        assert!(with_extra.is_valid());
    }

    #[test]
    fn maps_codex_permission_requests_to_status_only_events() {
        let payloads: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/codex-permission-request.json"))
                .expect("fixture JSON is valid");
        let valid = parse_event(&payloads["valid"].to_string()).expect("valid hook payload");
        let with_extra = parse_event(&payloads["unknownFields"].to_string())
            .expect("unknown fields are ignored");

        assert_eq!(valid.source, "ai");
        assert_eq!(valid.agent.as_deref(), Some("codex"));
        assert_eq!(
            valid.session_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(valid.status.as_deref(), Some("needs_input"));
        assert!(valid.actions.is_none());
        assert!(valid.body.is_none());
        assert_eq!(with_extra.status.as_deref(), Some("needs_input"));
        assert!(parse_event(&payloads["malformed"].to_string()).is_none());

        let with_process = r#"{
            "hook_event_name":"PermissionRequest",
            "session_id":"550e8400-e29b-41d4-a716-446655440000",
            "pid":4321
        }"#;
        assert_eq!(
            parse_event(with_process).and_then(|event| event.pid),
            Some(4321)
        );
    }

    #[test]
    fn accepts_schema_v2_ai_snapshots_and_rejects_unknown_agents() {
        let payload = r#"{
            "id":"550e8400-e29b-41d4-a716-446655440000",
            "source":"ai",
            "agent":"codex",
            "sessionId":"thread-123",
            "kind":"task",
            "status":"running",
            "title":"Refactor auth",
            "usage":{"tokens":18200,"quotaLeft":0.63},
            "extensionField":"ignored"
        }"#;
        let parsed = parse_event(payload).expect("valid v2 task snapshot");
        assert_eq!(parsed.agent.as_deref(), Some("codex"));
        assert_eq!(parsed.session_id.as_deref(), Some("thread-123"));
        assert!(parse_event(&payload.replace("codex", "unknown-agent")).is_none());
        let app = parse_event(&payload.replace("codex", "antigravity-app"))
            .expect("Antigravity app task is supported");
        assert_eq!(app.agent.as_deref(), Some("antigravity-app"));
    }

    #[test]
    fn permits_id_fallback_when_session_identity_is_missing() {
        let payload = r#"{
            "id":"550e8400-e29b-41d4-a716-446655440000",
            "source":"ai",
            "kind":"task",
            "status":"running",
            "title":"Refactor auth"
        }"#;
        assert!(parse_event(payload).is_some());
    }

    #[test]
    fn rejects_malformed_payloads() {
        let payloads: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/codex-notify.json"))
                .expect("fixture JSON is valid");
        let malformed = parse_event(&payloads["malformed"].to_string());

        assert!(malformed.is_none());
    }

    #[test]
    fn validates_uuid_shape() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_uuid("not-a-uuid"));
    }

    #[test]
    fn validates_iso8601_timestamps() {
        assert!(super::is_iso8601("2026-10-09T12:00:00Z"));
        assert!(super::is_iso8601("2026-10-09T12:00:00.123+08:00"));
        assert!(!super::is_iso8601("yesterday"));
    }
}
