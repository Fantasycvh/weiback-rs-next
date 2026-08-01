//! JSONL stdio 协议 v1 的解析与序列化。
//!
//! 协议定义见 `docs/protocol/v1/command.schema.json`、`event.schema.json`
//! 和 `dtos.schema.json`。本模块只负责信封层：校验协议版本、UUID v7、
//! 消息类型，以及信封的序列化/反序列化。payload 以 [`serde_json::Value`]
//! 保留，由上层按事件类型消费。
use std::fmt;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

/// 协议版本，首期固定为 1。
pub const PROTOCOL_VERSION: u64 = 1;
/// 单条消息大小上限（字节），与 `docs/protocol/v1/README.md` 一致。
pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;

/// 命令类型，与 `command.schema.json` 的 enum 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandType {
    Hello,
    Health,
    CollectUserPosts,
    CollectComments,
    CollectCommentReplies,
    Cancel,
    Shutdown,
}

impl CommandType {
    /// 所有合法命令类型。
    pub const ALL: &'static [CommandType] = &[
        CommandType::Hello,
        CommandType::Health,
        CommandType::CollectUserPosts,
        CommandType::CollectComments,
        CommandType::CollectCommentReplies,
        CommandType::Cancel,
        CommandType::Shutdown,
    ];

    /// 解析命令类型字符串。
    pub fn parse(s: &str) -> Option<CommandType> {
        CommandType::ALL.iter().copied().find(|c| c.as_str() == s)
    }

    /// 命令类型的 snake_case 字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            CommandType::Hello => "hello",
            CommandType::Health => "health",
            CommandType::CollectUserPosts => "collect_user_posts",
            CommandType::CollectComments => "collect_comments",
            CommandType::CollectCommentReplies => "collect_comment_replies",
            CommandType::Cancel => "cancel",
            CommandType::Shutdown => "shutdown",
        }
    }
}

impl fmt::Display for CommandType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 事件类型，与 `event.schema.json` 的 enum 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Ready,
    Capabilities,
    Started,
    Progress,
    User,
    Post,
    Comment,
    MediaReference,
    Checkpoint,
    RateLimited,
    AuthRequired,
    Warning,
    Error,
    Done,
    Cancelled,
}

impl EventType {
    /// 所有合法事件类型。
    pub const ALL: &'static [EventType] = &[
        EventType::Ready,
        EventType::Capabilities,
        EventType::Started,
        EventType::Progress,
        EventType::User,
        EventType::Post,
        EventType::Comment,
        EventType::MediaReference,
        EventType::Checkpoint,
        EventType::RateLimited,
        EventType::AuthRequired,
        EventType::Warning,
        EventType::Error,
        EventType::Done,
        EventType::Cancelled,
    ];

    /// 解析事件类型字符串。
    pub fn parse(s: &str) -> Option<EventType> {
        EventType::ALL.iter().copied().find(|e| e.as_str() == s)
    }

    /// 事件类型的 snake_case 字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Ready => "ready",
            EventType::Capabilities => "capabilities",
            EventType::Started => "started",
            EventType::Progress => "progress",
            EventType::User => "user",
            EventType::Post => "post",
            EventType::Comment => "comment",
            EventType::MediaReference => "media_reference",
            EventType::Checkpoint => "checkpoint",
            EventType::RateLimited => "rate_limited",
            EventType::AuthRequired => "auth_required",
            EventType::Warning => "warning",
            EventType::Error => "error",
            EventType::Done => "done",
            EventType::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// UUID v7 正则。
static UUID_V7_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

fn uuid_v7_re() -> &'static Regex {
    UUID_V7_RE.get_or_init(|| {
        Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .expect("uuid v7 regex is valid")
    })
}

/// 检查字符串是否为合法 UUID v7。
pub fn is_uuid_v7(s: &str) -> bool {
    uuid_v7_re().is_match(s)
}

/// 生成一个新的 UUID v7 字符串。
pub fn new_uuid_v7() -> String {
    Uuid::now_v7().to_string()
}

/// 协议层错误。
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// 消息不是合法 JSON。
    #[error("invalid json line: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// 顶层不是 JSON 对象。
    #[error("message must be a json object")]
    NotAnObject,
    /// 缺少必填字段。
    #[error("missing required field: {0}")]
    MissingField(String),
    /// 字段类型错误。
    #[error("field {field} has wrong type: {detail}")]
    WrongType { field: String, detail: String },
    /// 协议版本不受支持。
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u64),
    /// request_id 不是合法 UUID v7。
    #[error("request_id is not a uuid v7: {0}")]
    InvalidRequestId(String),
    /// event_id 不是合法 UUID v7。
    #[error("event_id is not a uuid v7: {0}")]
    InvalidEventId(String),
    /// 消息超过大小上限。
    #[error("message exceeds {MAX_MESSAGE_BYTES} bytes")]
    MessageTooLarge,
    /// 未知的命令类型。
    #[error("unknown command type: {0}")]
    UnknownCommand(String),
    /// 未知的事件类型。
    #[error("unknown event type: {0}")]
    UnknownEvent(String),
}

/// Rust 到 Sidecar 的命令信封。
///
/// 序列化时按 `command.schema.json` 输出，行尾追加 `\n`。
#[derive(Debug, Clone, PartialEq)]
pub struct CommandEnvelope {
    /// 协议版本，固定为 1。
    pub protocol_version: u64,
    /// 一次采集请求的关联 ID（UUID v7）。
    pub request_id: String,
    /// 命令类型。
    pub command_type: CommandType,
    /// 命令 payload；无 payload 时为空对象。
    pub payload: Value,
}

impl CommandEnvelope {
    /// 创建一个命令信封。
    pub fn new(request_id: String, command_type: CommandType, payload: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            command_type,
            payload,
        }
    }

    /// 序列化为单行 JSONL。
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        let mut obj = serde_json::Map::new();
        obj.insert("protocol_version".into(), json!(self.protocol_version));
        obj.insert("request_id".into(), json!(self.request_id));
        obj.insert("type".into(), json!(self.command_type.as_str()));
        obj.insert("payload".into(), self.payload.clone());
        let mut line = serde_json::to_string(&Value::Object(obj))?;
        line.push('\n');
        Ok(line)
    }
}

/// Sidecar 到 Rust 的事件信封。
#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    /// 协议版本，固定为 1。
    pub protocol_version: u64,
    /// 关联的命令 request_id；握手/生命周期事件可为 null。
    pub request_id: Option<String>,
    /// 单个事件的全局幂等键（UUID v7）。
    pub event_id: String,
    /// 事件类型。
    pub event_type: EventType,
    /// 逻辑资源流；事件相关时存在。
    pub stream: Option<String>,
    /// 同一请求、同一 stream 内从 1 开始单调递增。
    pub sequence: Option<u64>,
    /// 当前可见总数的估算，可为 null。
    pub total_expected: Option<u64>,
    /// Sidecar 生成事件的 UTC 时间。
    pub occurred_at: String,
    /// 事件类型对应的数据。
    pub payload: Value,
}

/// 从一行 JSONL 解析事件信封。
///
/// 逐行解析；单条非法 JSON 只产生 [`ProtocolError`]，不会污染数据库。
pub fn parse_event_line(line: &str) -> Result<EventEnvelope, ProtocolError> {
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge);
    }
    let value: Value = serde_json::from_str(line)?;
    let obj = value.as_object().ok_or(ProtocolError::NotAnObject)?;

    let version = get_u64(obj, "protocol_version")?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }

    let event_type = get_str(obj, "type").ok_or_else(|| {
        ProtocolError::MissingField("type".to_string())
    })?;
    let event_type = EventType::parse(event_type)
        .ok_or_else(|| ProtocolError::UnknownEvent(event_type.to_string()))?;

    let request_id = get_opt_str(obj, "request_id");
    if let Some(rid) = &request_id
        && !is_uuid_v7(rid)
    {
        return Err(ProtocolError::InvalidRequestId(rid.to_string()));
    }

    let event_id = get_str(obj, "event_id").ok_or_else(|| {
        ProtocolError::MissingField("event_id".to_string())
    })?;
    if !is_uuid_v7(event_id) {
        return Err(ProtocolError::InvalidEventId(event_id.to_string()));
    }

    let occurred_at = get_str(obj, "occurred_at")
        .ok_or_else(|| ProtocolError::MissingField("occurred_at".to_string()))?
        .to_string();

    let payload = obj
        .get("payload")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    Ok(EventEnvelope {
        protocol_version: version,
        request_id: request_id.map(String::from),
        event_id: event_id.to_string(),
        event_type,
        stream: get_opt_str(obj, "stream").map(String::from),
        sequence: get_opt_u64(obj, "sequence"),
        total_expected: get_opt_u64(obj, "total_expected"),
        occurred_at,
        payload,
    })
}

/// 从一行 JSONL 解析命令信封（用于测试与调试）。
pub fn parse_command_line(line: &str) -> Result<CommandEnvelope, ProtocolError> {
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge);
    }
    let value: Value = serde_json::from_str(line)?;
    let obj = value.as_object().ok_or(ProtocolError::NotAnObject)?;

    let version = get_u64(obj, "protocol_version")?;
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }

    let command_type = get_str(obj, "type")
        .ok_or_else(|| ProtocolError::MissingField("type".to_string()))?;
    let command_type = CommandType::parse(command_type)
        .ok_or_else(|| ProtocolError::UnknownCommand(command_type.to_string()))?;

    let request_id = get_str(obj, "request_id")
        .ok_or_else(|| ProtocolError::MissingField("request_id".to_string()))?;
    if !is_uuid_v7(request_id) {
        return Err(ProtocolError::InvalidRequestId(request_id.to_string()));
    }

    let payload = obj
        .get("payload")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    Ok(CommandEnvelope {
        protocol_version: version,
        request_id: request_id.to_string(),
        command_type,
        payload,
    })
}

fn get_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

fn get_opt_str<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key)
        .filter(|v| !v.is_null())
        .and_then(Value::as_str)
}

fn get_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Result<u64, ProtocolError> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| ProtocolError::MissingField(key.to_string()))
}

fn get_opt_u64(obj: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    obj.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid() -> String {
        new_uuid_v7()
    }

    #[test]
    fn command_envelope_round_trip() {
        let envelope = CommandEnvelope::new(
            rid(),
            CommandType::CollectComments,
            json!({"post_id": "123", "max_pages": 3}),
        );
        let line = envelope.to_line().unwrap();
        let parsed = parse_command_line(&line).unwrap();
        assert_eq!(parsed, envelope);
    }

    #[test]
    fn command_line_ends_with_newline() {
        let envelope = CommandEnvelope::new(rid(), CommandType::Health, json!({}));
        let line = envelope.to_line().unwrap();
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn event_line_parses_ready() {
        let line = json!({
            "protocol_version": 1,
            "request_id": null,
            "event_id": rid(),
            "type": "ready",
            "occurred_at": "2026-07-31T12:34:56.789Z",
            "payload": {
                "sidecar_name": "weiback-collector",
                "sidecar_version": "0.3.1",
                "protocol_version": 1
            }
        })
        .to_string();
        let event = parse_event_line(&line).unwrap();
        assert_eq!(event.event_type, EventType::Ready);
        assert_eq!(event.request_id, None);
        assert_eq!(event.payload["sidecar_name"], "weiback-collector");
    }

    #[test]
    fn event_line_parses_stream_event() {
        let line = json!({
            "protocol_version": 1,
            "request_id": rid(),
            "event_id": rid(),
            "type": "post",
            "stream": "user:123:posts",
            "sequence": 42,
            "total_expected": 500,
            "occurred_at": "2026-07-31T12:34:56.789Z",
            "payload": {"id": "42", "uid": "123"}
        })
        .to_string();
        let event = parse_event_line(&line).unwrap();
        assert_eq!(event.event_type, EventType::Post);
        assert_eq!(event.sequence, Some(42));
        assert_eq!(event.total_expected, Some(500));
        assert_eq!(event.stream.as_deref(), Some("user:123:posts"));
    }

    #[test]
    fn rejects_wrong_protocol_version() {
        let line = json!({
            "protocol_version": 2,
            "request_id": null,
            "event_id": rid(),
            "type": "ready",
            "occurred_at": "2026-07-31T12:34:56.789Z",
            "payload": {}
        })
        .to_string();
        let err = parse_event_line(&line).unwrap_err();
        assert!(matches!(err, ProtocolError::UnsupportedVersion(2)));
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(matches!(
            parse_event_line("not json"),
            Err(ProtocolError::InvalidJson(_))
        ));
    }

    #[test]
    fn rejects_non_object() {
        assert!(matches!(
            parse_event_line("[1,2,3]"),
            Err(ProtocolError::NotAnObject)
        ));
    }

    #[test]
    fn rejects_unknown_event_type() {
        let line = json!({
            "protocol_version": 1,
            "request_id": null,
            "event_id": rid(),
            "type": "nope",
            "occurred_at": "2026-07-31T12:34:56.789Z",
            "payload": {}
        })
        .to_string();
        assert!(matches!(
            parse_event_line(&line),
            Err(ProtocolError::UnknownEvent(_))
        ));
    }

    #[test]
    fn rejects_invalid_event_id() {
        let line = json!({
            "protocol_version": 1,
            "request_id": null,
            "event_id": "not-a-uuid",
            "type": "ready",
            "occurred_at": "2026-07-31T12:34:56.789Z",
            "payload": {}
        })
        .to_string();
        assert!(matches!(
            parse_event_line(&line),
            Err(ProtocolError::InvalidEventId(_))
        ));
    }

    #[test]
    fn rejects_missing_occurred_at() {
        let line = json!({
            "protocol_version": 1,
            "request_id": null,
            "event_id": rid(),
            "type": "ready",
            "payload": {}
        })
        .to_string();
        assert!(matches!(
            parse_event_line(&line),
            Err(ProtocolError::MissingField(_))
        ));
    }

    #[test]
    fn rejects_message_too_large() {
        let line = " ".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(matches!(
            parse_event_line(&line),
            Err(ProtocolError::MessageTooLarge)
        ));
    }

    #[test]
    fn uuid_v7_validation() {
        let good = rid();
        assert!(is_uuid_v7(&good));
        assert!(!is_uuid_v7("0198-0000-0000-0000-0000"));
        assert!(!is_uuid_v7(&good.replace('7', "8")));
    }
}
