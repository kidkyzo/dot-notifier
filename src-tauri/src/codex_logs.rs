use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::de::IgnoredAny;
use serde::Deserialize;
use tauri::{AppHandle, Emitter};

use crate::bus::Event;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DISCOVERY_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const ACTIVE_AGE: Duration = Duration::from_secs(3 * 60);
const INITIAL_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskState {
    Running,
    Done,
    Error,
}

struct WatchedFile {
    length: u64,
    thread_id: String,
}

#[derive(Deserialize)]
struct RolloutLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    record_type: String,
    payload: Option<RolloutPayload>,
}

#[derive(Deserialize)]
struct RolloutPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    #[serde(default)]
    error: Option<IgnoredAny>,
}

#[derive(Debug, PartialEq, Eq)]
struct LogUpdate {
    state: TaskState,
    title: &'static str,
    started_at: Option<String>,
}

pub fn start(app: AppHandle, codex_enabled: Arc<AtomicBool>) {
    if let Err(error) = thread::Builder::new()
        .name("dot-notifier-codex-logs".into())
        .spawn(move || watch_sessions(app, codex_enabled))
    {
        eprintln!("Could not start Codex log adapter: {error}");
    }
}

fn watch_sessions(app: AppHandle, codex_enabled: Arc<AtomicBool>) {
    let Some(sessions_dir) = sessions_directory() else {
        return;
    };

    let mut watched = HashMap::<PathBuf, WatchedFile>::new();
    let mut was_enabled = false;
    thread::sleep(POLL_INTERVAL);
    loop {
        if !codex_enabled.load(Ordering::Acquire) {
            watched.clear();
            was_enabled = false;
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        if !was_enabled {
            watched.clear();
            was_enabled = true;
        }
        for path in recent_rollouts(&sessions_dir) {
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            let length = metadata.len();

            if let Some(file_state) = watched.get_mut(&path) {
                if length < file_state.length {
                    file_state.length = length;
                }
                if length > file_state.length {
                    let updates = read_updates(&path, file_state.length, length);
                    file_state.length = length;
                    for update in updates {
                        emit_task(&app, &file_state.thread_id, update);
                    }
                }
                continue;
            }

            let Some(thread_id) = thread_id_from_path(&path) else {
                continue;
            };
            let initial_offset = length.saturating_sub(INITIAL_TAIL_BYTES);
            let updates = read_updates(&path, initial_offset, length);
            if let Some(latest) = updates.last() {
                if latest.state == TaskState::Running
                    && (latest
                        .started_at
                        .as_deref()
                        .is_some_and(|timestamp| timestamp_is_recent(timestamp, ACTIVE_AGE))
                        || metadata
                            .modified()
                            .ok()
                            .and_then(|time| SystemTime::now().duration_since(time).ok())
                            .is_some_and(|age| age <= ACTIVE_AGE))
                {
                    emit_task(
                        &app,
                        &thread_id,
                        LogUpdate {
                            state: TaskState::Running,
                            title: "Codex task running",
                            started_at: latest.started_at.clone(),
                        },
                    );
                }
            }
            watched.insert(path, WatchedFile { length, thread_id });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn timestamp_is_recent(timestamp: &str, max_age: Duration) -> bool {
    let Some((date, time_and_zone)) = timestamp.split_once('T') else {
        return false;
    };
    let mut date_parts = date.split('-');
    let (Some(Ok(year)), Some(Ok(month)), Some(Ok(day)), None) = (
        date_parts.next().map(str::parse::<i64>),
        date_parts.next().map(str::parse::<i64>),
        date_parts.next().map(str::parse::<i64>),
        date_parts.next(),
    ) else {
        return false;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }

    let zone_start = time_and_zone
        .find(['Z', '+', '-'])
        .unwrap_or(time_and_zone.len());
    let time = &time_and_zone[..zone_start];
    let mut time_parts = time.split(':');
    let (Some(Ok(hour)), Some(Ok(minute)), Some(second_text), None) = (
        time_parts.next().map(str::parse::<i64>),
        time_parts.next().map(str::parse::<i64>),
        time_parts.next(),
        time_parts.next(),
    ) else {
        return false;
    };
    let second = second_text
        .split('.')
        .next()
        .and_then(|part| part.parse::<i64>().ok());
    let (Some(second), true) = (
        second,
        (0..=23).contains(&hour) && (0..=59).contains(&minute),
    ) else {
        return false;
    };
    if !(0..=60).contains(&second) {
        return false;
    }

    let zone = &time_and_zone[zone_start..];
    let offset_seconds = if zone.is_empty() || zone == "Z" {
        0
    } else {
        let sign = match zone.as_bytes().first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return false,
        };
        let mut offset_parts = zone[1..].split(':');
        let (Some(Ok(offset_hour)), Some(Ok(offset_minute)), None) = (
            offset_parts.next().map(str::parse::<i64>),
            offset_parts.next().map(str::parse::<i64>),
            offset_parts.next(),
        ) else {
            return false;
        };
        if !(0..=23).contains(&offset_hour) || !(0..=59).contains(&offset_minute) {
            return false;
        }
        sign * (offset_hour * 3600 + offset_minute * 60)
    };

    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    let unix_seconds =
        days_since_epoch * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds;
    let Ok(unix_seconds) = u64::try_from(unix_seconds) else {
        return false;
    };
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .is_some_and(|now| {
            now.as_secs().saturating_sub(unix_seconds) <= max_age.as_secs()
                && unix_seconds <= now.as_secs()
        })
}

fn sessions_directory() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex"))
        })?;
    Some(home.join("sessions"))
}

fn recent_rollouts(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path.file_name().is_some_and(|name| {
                    name.to_string_lossy().starts_with("rollout-")
                        && name.to_string_lossy().ends_with(".jsonl")
                })
                && metadata
                    .modified()
                    .ok()
                    .and_then(|time| SystemTime::now().duration_since(time).ok())
                    .is_some_and(|age| age <= DISCOVERY_AGE)
            {
                found.push(path);
            }
        }
    }
    found
}

fn thread_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    let suffix = stem.get(stem.len().checked_sub(36)?..)?;
    is_uuid(suffix).then(|| suffix.to_ascii_lowercase())
}

fn read_updates(path: &Path, offset: u64, length: u64) -> Vec<LogUpdate> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(length.saturating_sub(offset) as usize);
    if file
        .take(length.saturating_sub(offset))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut updates = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(update) = parse_rollout_line(line) {
            updates.push(update);
        }
    }
    updates
}

fn parse_rollout_line(line: &str) -> Option<LogUpdate> {
    let record: RolloutLine = serde_json::from_str(line).ok()?;
    if record.record_type != "event_msg" {
        return None;
    }
    let started_at = record.timestamp;
    let payload = record.payload?;
    match payload.event_type.as_deref()? {
        "task_started" => Some(LogUpdate {
            state: TaskState::Running,
            title: "Codex task running",
            started_at,
        }),
        "task_complete" if payload.error.is_some() => Some(LogUpdate {
            state: TaskState::Error,
            title: "Codex task failed",
            started_at: None,
        }),
        "task_complete" | "turn_aborted" => Some(LogUpdate {
            state: TaskState::Done,
            title: "Codex task completed",
            started_at: None,
        }),
        _ => None,
    }
}

fn emit_task(app: &AppHandle, thread_id: &str, update: LogUpdate) {
    let status = match update.state {
        TaskState::Running => "running",
        TaskState::Done => "done",
        TaskState::Error => "error",
    };
    let event = Event {
        id: thread_id.to_owned(),
        source: "ai".into(),
        agent: Some("codex".into()),
        session_id: Some(thread_id.to_owned()),
        kind: "task".into(),
        status: Some(status.into()),
        title: update.title.into(),
        body: None,
        progress: None,
        started_at: update.started_at,
        cwd: None,
        pid: None,
        usage: None,
        actions: None,
        action_nonce: None,
        meta: None,
    };
    if let Err(error) = app.emit("bus-event", event) {
        eprintln!("Could not deliver Codex log update to UI: {error}");
    }
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

#[cfg(test)]
mod tests {
    use super::{parse_rollout_line, thread_id_from_path, LogUpdate, TaskState};
    use std::path::Path;

    #[test]
    fn parses_lifecycle_markers_without_reading_transcript_text() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/codex-rollout.json"))
                .expect("fixture JSON is valid");
        assert_eq!(
            parse_rollout_line(fixture["started"].as_str().expect("line fixture")),
            Some(LogUpdate {
                state: TaskState::Running,
                title: "Codex task running",
                started_at: Some("2026-10-09T12:00:00Z".into()),
            })
        );
        assert_eq!(
            parse_rollout_line(fixture["failed"].as_str().expect("line fixture")),
            Some(LogUpdate {
                state: TaskState::Error,
                title: "Codex task failed",
                started_at: None,
            })
        );
        assert_eq!(
            parse_rollout_line(fixture["unknownFields"].as_str().expect("line fixture")),
            Some(LogUpdate {
                state: TaskState::Done,
                title: "Codex task completed",
                started_at: None,
            })
        );
    }

    #[test]
    fn drops_malformed_or_unrelated_records() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/codex-rollout.json"))
                .expect("fixture JSON is valid");
        assert!(parse_rollout_line(fixture["malformed"].as_str().expect("line fixture")).is_none());
        assert!(parse_rollout_line(fixture["message"].as_str().expect("line fixture")).is_none());
    }

    #[test]
    fn extracts_thread_uuid_from_rollout_filename() {
        assert_eq!(
            thread_id_from_path(Path::new(
                "rollout-2026-10-09T12-00-00-550e8400-e29b-41d4-a716-446655440000.jsonl"
            )),
            Some("550e8400-e29b-41d4-a716-446655440000".into())
        );
        assert_eq!(
            thread_id_from_path(Path::new("rollout-invalid.jsonl")),
            None
        );
    }
}
