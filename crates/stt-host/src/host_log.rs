use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const QUEUE_CAPACITY: usize = 512;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 4096;
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum HostLogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

impl HostLogLevel {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("trace") {
            Some(Self::Trace)
        } else if value.eq_ignore_ascii_case("debug") {
            Some(Self::Debug)
        } else if value.eq_ignore_ascii_case("info") {
            Some(Self::Info)
        } else if value.eq_ignore_ascii_case("warn") {
            Some(Self::Warn)
        } else if value.eq_ignore_ascii_case("error") {
            Some(Self::Error)
        } else {
            None
        }
    }

    pub(super) const fn allows(self, event: Self) -> bool {
        event as u8 >= self as u8
    }

    pub(super) fn infer_legacy(line: &str) -> Self {
        if contains_ascii_case_insensitive(line, "panic")
            || contains_ascii_case_insensitive(line, "failed")
            || contains_ascii_case_insensitive(line, "error")
            || contains_ascii_case_insensitive(line, "spawn_err")
            || contains_ascii_case_insensitive(line, "=err")
            || contains_ascii_case_insensitive(line, " err ")
            || ends_with_ascii_case_insensitive(line, " err")
            || contains_ascii_case_insensitive(line, "unresolved")
        {
            Self::Error
        } else if contains_ascii_case_insensitive(line, "unavailable")
            || contains_ascii_case_insensitive(line, "not attached")
            || contains_ascii_case_insensitive(line, "hook lost")
            || contains_ascii_case_insensitive(line, "waiting")
            || contains_ascii_case_insensitive(line, "fallback")
            || contains_ascii_case_insensitive(line, "legacy_flag")
            || contains_ascii_case_insensitive(line, "skip=")
            || contains_ascii_case_insensitive(line, "disabled")
            || contains_ascii_case_insensitive(line, " miss")
        {
            Self::Warn
        } else if contains_ascii_case_insensitive(line, "_stats ") {
            Self::Debug
        } else {
            Self::Info
        }
    }
}

struct LogRecord {
    timestamp_ms: u128,
    uptime_ms: u128,
    level: HostLogLevel,
    event: String,
    thread: String,
    message: String,
    dropped_before: u64,
}

pub(super) struct HostLogger {
    sender: SyncSender<LogRecord>,
    level: AtomicU8,
    started: Instant,
    dropped: AtomicU64,
}

impl HostLogger {
    pub(super) fn new(path: &Path, level: &str) -> io::Result<Self> {
        // 轮转失败不阻断宿主启动, 追加旧文件仍比丢失本次诊断信息好.
        let _ = rotate_log(path);
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let has_content = file.metadata().is_ok_and(|metadata| metadata.len() > 0);
        let started = Instant::now();
        let started_ms = unix_timestamp_ms();
        let session_id = format!(
            "stt-{}-{}-{}",
            std::process::id(),
            started_ms,
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let writer_session = session_id.clone();
        let writer_path = path.to_path_buf();
        thread::Builder::new()
            .name("log-writer".to_owned())
            .spawn(move || {
                run_writer(
                    writer_path,
                    file,
                    receiver,
                    writer_session,
                    started_ms,
                    has_content,
                )
            })?;

        Ok(Self {
            sender,
            level: AtomicU8::new(HostLogLevel::parse(level).unwrap_or(HostLogLevel::Debug) as u8),
            started,
            dropped: AtomicU64::new(0),
        })
    }

    pub(super) fn set_level(&self, level: &str) -> bool {
        let Some(level) = HostLogLevel::parse(level) else {
            return false;
        };
        self.level.store(level as u8, Ordering::Relaxed);
        true
    }

    pub(super) fn log(&self, steam_root: &Path, level: HostLogLevel, line: &str) {
        let configured = match self.level.load(Ordering::Relaxed) {
            0 => HostLogLevel::Trace,
            1 => HostLogLevel::Debug,
            2 => HostLogLevel::Info,
            3 => HostLogLevel::Warn,
            _ => HostLogLevel::Error,
        };
        if !configured.allows(level) {
            return;
        }

        let record = LogRecord {
            timestamp_ms: unix_timestamp_ms(),
            uptime_ms: self.started.elapsed().as_millis(),
            level,
            event: event_name(line),
            thread: thread_name(),
            message: sanitize_message(steam_root, line),
            dropped_before: self.dropped.swap(0, Ordering::Relaxed),
        };
        match self.sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn run_writer(
    path: PathBuf,
    file: File,
    receiver: mpsc::Receiver<LogRecord>,
    session_id: String,
    started_ms: u128,
    has_content: bool,
) {
    let mut writer = Some(BufWriter::new(file));
    if has_content && write_line(writer.as_mut().unwrap(), "").is_err() {
        return;
    }
    if write_line(
        writer.as_mut().unwrap(),
        &format!(
            "ts_unix_ms={started_ms} level=info target=stt_host event=session_start session={session_id} thread=log-writer uptime_ms=0 dropped_before=0 message=\"log session started\""
        ),
    )
    .is_err()
    {
        return;
    }

    while let Ok(record) = receiver.recv() {
        let line = format_record(&record, &session_id);
        if write_line(writer.as_mut().unwrap(), &line).is_err() {
            break;
        }
        // 每次写后看一眼文件大小, 超上限就地轮转, 别等下次启动才轮.
        let over_limit = writer.as_ref().is_some_and(|w| {
            w.get_ref()
                .metadata()
                .is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES)
        });
        if over_limit && rotate_writer(&mut writer, &path).is_err() {
            break;
        }
    }
}

/// 会话中就地轮转: 关掉旧句柄, 当前文件挪成 .log.1 (老 .log.1 顺延 .log.2),
/// 再在原名上开新文件继续写.
fn rotate_writer(writer: &mut Option<BufWriter<File>>, path: &Path) -> io::Result<()> {
    // 先落盘并关闭旧句柄, 不然 rename 会撞上仍打开的文件.
    if let Some(mut w) = writer.take() {
        w.flush()?;
    }
    rotate_log(path)?;
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    *writer = Some(BufWriter::new(file));
    Ok(())
}

fn write_line(writer: &mut BufWriter<File>, line: &str) -> io::Result<()> {
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn format_record(record: &LogRecord, session_id: &str) -> String {
    format!(
        "ts_unix_ms={} level={} target=stt_host event={} session={} thread={} uptime_ms={} dropped_before={} message=\"{}\"",
        record.timestamp_ms,
        record.level.as_str(),
        record.event,
        session_id,
        escape_field(&record.thread),
        record.uptime_ms,
        record.dropped_before,
        escape_field(&record.message)
    )
}

fn event_name(line: &str) -> String {
    let first = line.split_whitespace().next().unwrap_or("host");
    let event = first.split_once('=').map_or(first, |(event, _)| event);
    sanitize_token(event)
}

fn thread_name() -> String {
    thread::current().name().map_or_else(
        || format!("unnamed-{:?}", thread::current().id()),
        ToOwned::to_owned,
    )
}

fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value.as_bytes().windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.as_bytes())
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn ends_with_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    value
        .as_bytes()
        .get(value.len().saturating_sub(needle.len())..)
        .is_some_and(|suffix| {
            suffix
                .iter()
                .zip(needle.as_bytes())
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

fn rotate_log(path: &Path) -> io::Result<()> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.len() < MAX_LOG_BYTES {
        return Ok(());
    }

    let first = path.with_extension("log.1");
    let second = path.with_extension("log.2");
    if second.is_file() {
        fs::remove_file(&second)?;
    }
    if first.is_file() {
        fs::rename(&first, &second)?;
    }
    fs::rename(path, first)
}

fn sanitize_message(steam_root: &Path, line: &str) -> String {
    // 先截断再脱敏: 超大输入 (>4 KiB) 先脱敏要反复全量扫描, 白白烧 CPU,
    // 而且截掉的部分本来就是丢弃的, 脱敏了也没人看.
    let mut message = truncate_message(line.to_owned());
    let root = steam_root.to_string_lossy();
    if !root.is_empty() {
        message = message.replace(root.as_ref(), "<steam_root>");
        let slash_root = root.replace('\\', "/");
        if slash_root != root {
            message = message.replace(&slash_root, "<steam_root>");
        }
    }
    redact_sensitive_fields(&mut message);
    message
}

fn redact_sensitive_fields(message: &mut String) {
    const FIELDS: [&str; 13] = [
        "key",
        "token",
        "ticket",
        "access_token",
        "app_access_token",
        "decryption_key",
        "authorization",
        "cookie",
        "password",
        "secret",
        "account",
        "username",
        "request_code",
    ];

    for field in FIELDS {
        redact_field(message, field);
        redact_json_field(message, field);
    }
}

fn redact_field(message: &mut String, field: &str) {
    let needle = format!("{field}=");
    let mut search_from = 0;
    while let Some(relative) = message[search_from..].find(&needle) {
        let start = search_from + relative;
        let boundary = start == 0
            || (!message.as_bytes()[start - 1].is_ascii_alphanumeric()
                && message.as_bytes()[start - 1] != b'_');
        if !boundary {
            search_from = start + needle.len();
            continue;
        }

        let value_start = start + needle.len();
        if value_start >= message.len() {
            break;
        }
        let quoted = message.as_bytes()[value_start] == b'"';
        let content_start = if quoted { value_start + 1 } else { value_start };
        let content_end = if quoted {
            quoted_content_end(message, content_start)
        } else {
            message[content_start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ')' | ']' | '}' | ';'))
                .map_or(message.len(), |offset| content_start + offset)
        };
        let value = &message[content_start..content_end];
        if field == "request_code" && matches!(value, "resolved" | "unresolved") {
            search_from = content_end;
            continue;
        }
        message.replace_range(content_start..content_end, "<redacted>");
        search_from = content_start + "<redacted>".len();
    }
}

fn redact_json_field(message: &mut String, field: &str) {
    let needle = format!("\"{field}\"");
    let mut search_from = 0;
    while let Some(relative) = message[search_from..].find(&needle) {
        let start = search_from + relative;
        let mut value_start = start + needle.len();
        while value_start < message.len() && message.as_bytes()[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        if !message[value_start..].starts_with(':') {
            search_from = value_start;
            continue;
        }
        value_start += 1;
        while value_start < message.len() && message.as_bytes()[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        if value_start >= message.len() {
            break;
        }
        let quoted = message.as_bytes()[value_start] == b'"';
        let content_start = if quoted { value_start + 1 } else { value_start };
        let content_end = if quoted {
            quoted_content_end(message, content_start)
        } else {
            message[content_start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '}' | ']'))
                .map_or(message.len(), |offset| content_start + offset)
        };
        let value = &message[content_start..content_end];
        if field == "request_code" && matches!(value, "resolved" | "unresolved") {
            search_from = content_end;
            continue;
        }
        message.replace_range(content_start..content_end, "<redacted>");
        search_from = content_start + "<redacted>".len();
    }
}

fn quoted_content_end(message: &str, content_start: usize) -> usize {
    let bytes = message.as_bytes();
    let mut index = content_start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'"' => return index,
            _ => index += 1,
        }
    }
    message.len()
}

fn truncate_message(mut message: String) -> String {
    if message.len() <= MAX_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_MESSAGE_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("...");
    message
}

fn sanitize_token(token: &str) -> String {
    token
        .chars()
        .take(64)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn escape_field(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            // 其余控制字符 (0x00-0x1F 除 \t) 与 0x7F 一律转义成 \xNN,
            // 免得 ESC/NUL 这类不可见字节混进日志文件.
            ch if ch <= '\u{1f}' || ch == '\u{7f}' => {
                let code = ch as u32;
                escaped.push_str("\\x");
                escaped.push(HEX[(code >> 4) as usize] as char);
                escaped.push(HEX[(code & 0xf) as usize] as char);
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{
        event_name, format_record, redact_sensitive_fields, sanitize_message, HostLogLevel,
        LogRecord,
    };
    use std::path::Path;

    #[test]
    fn log_level_parser_accepts_known_values_only() {
        assert_eq!(HostLogLevel::parse("WARN"), Some(HostLogLevel::Warn));
        assert_eq!(HostLogLevel::parse("verbose"), None);
    }

    #[test]
    fn legacy_level_inference_marks_failures_and_stats() {
        assert_eq!(
            HostLogLevel::infer_legacy("catalog_add=err result=failure"),
            HostLogLevel::Error
        );
        assert_eq!(
            HostLogLevel::infer_legacy("download_key_stats calls=1"),
            HostLogLevel::Debug
        );
    }

    #[test]
    fn event_name_uses_the_legacy_key() {
        assert_eq!(event_name("catalog_add=ok app_id=42"), "catalog_add");
        assert_eq!(event_name("SteamTools host init"), "SteamTools");
    }

    #[test]
    fn sensitive_fields_are_redacted_without_matching_target_names() {
        let mut message = concat!(
            "download_key=hook attached key=",
            "abababababababababababababababababababababababababababababababab",
            " token=123 request_code=resolved"
        )
        .to_owned();
        redact_sensitive_fields(&mut message);
        assert!(message.contains("download_key=hook attached"));
        assert!(message.contains("key=<redacted>"));
        assert!(message.contains("token=<redacted>"));
        assert!(message.contains("request_code=resolved"));
    }

    #[test]
    fn json_sensitive_fields_are_redacted() {
        let mut message =
            r#"{"key":"secret","access_token":123,"request_code":"resolved"}"#.to_owned();
        redact_sensitive_fields(&mut message);
        assert!(message.contains(r#""key":"<redacted>""#));
        assert!(message.contains(r#""access_token":<redacted>"#));
        assert!(message.contains(r#""request_code":"resolved""#));
    }

    #[test]
    fn escaped_json_sensitive_values_are_redacted_as_one_value() {
        let mut message = r#"{"key":"a\"secret"}"#.to_owned();
        redact_sensitive_fields(&mut message);
        assert!(message.contains(r#""key":"<redacted>""#));
        assert!(!message.contains("secret"));
    }

    #[test]
    fn steam_root_is_replaced_before_writing_message() {
        let message = sanitize_message(
            Path::new("fixture-root"),
            r#"path=fixture-root\steamtools\host.log" key=secret"#,
        );
        assert!(message.contains("path=<steam_root>\\steamtools\\host.log"));
        assert!(!message.contains("fixture-root"));
        assert!(message.contains("key=<redacted>"));
    }

    #[test]
    fn formatted_record_contains_session_and_runtime_context() {
        let record = LogRecord {
            timestamp_ms: 1,
            uptime_ms: 2,
            level: HostLogLevel::Info,
            event: "status".to_owned(),
            thread: "watch".to_owned(),
            message: "status=ready \"ok\"".to_owned(),
            dropped_before: 3,
        };
        let line = format_record(&record, "session-1");
        assert!(line.contains("target=stt_host"));
        assert!(line.contains("session=session-1"));
        assert!(line.contains("thread=watch"));
        assert!(line.contains("dropped_before=3"));
        assert!(line.contains("message=\"status=ready \\\"ok\\\"\""));
    }
}
