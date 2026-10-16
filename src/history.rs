//! AI chat-history persistence (`history` feature): an owned SQLite store
//! (`chat_histories.db`) written through ormer's turso backend, replicating
//! the ZCode session-store table format (`session` / `message` / `part` /
//! `session_entry` / `local_setting` + the channel-side `project` extension
//! table). ZCode columns channel never writes are created anyway and stay
//! NULL, so the same file stays interchangeable with a ZCode-managed store.
//!
//! Timestamps are epoch milliseconds (`i64` rows, `u64` records). The
//! `data` columns carry ZCode-keyed JSON produced by the composition
//! functions below; hosts convert [`ProjectRecord`] / [`SessionRecord`] /
//! [`MessageRecord`] into their own DTOs.
//!
//! # Red lines (carried over verbatim from the design spec, do not drop)
//!
//! 1. **turso 只连自有库**：ormer 底层是 turso 引擎，非标准 SQLite 锁协议；
//!    `chat_histories.db` 由本模块自己创建/打开，属"自有库"，直连安全。
//!    （外部标准 SQLite 库——如 ZCode 引擎库——一律走 rusqlite 只读连接，
//!    见 [`zcode_session_db_path`] / [`read_zcode_compaction_summary_text`]。）
//! 2. **journal_mode 是查询型 PRAGMA**：`Connection::execute` 遇结果行必报
//!    `unexpected row during execution`，但副作用已生效——必须走查询通道
//!    （`select_sql::<String>` 收 `Vec<String>`）消费结果行并校验返回值；
//!    `foreign_keys` / `busy_timeout` 带 RHS 时为零结果列，走 execute。
//! 3. **模型不声明 `#[foreign]`**：SQLite 不能 ALTER ADD CONSTRAINT，ormer 对
//!    已含外键的存量表声明了反而报 unmigratable schema；不声明则保留存量外键。
//!    级联删除一律走显式"先子后父"事务。
//! 4. **单连接并发 forbidden**：ormer SQLite `Database` 内部是单条 turso 连接，
//!    同一连接上两个查询执行窗口重叠报 `concurrent use forbidden`。所有走
//!    句柄的操作必须在内部互斥体上排队；`tokio::sync::Mutex` 不可重入，
//!    组合操作复用锁内 `&Database` 调底层函数。
//! 5. **`type` 是 Rust 关键字**：`SessionEntryRow.entry_type` 用
//!    `#[column(name = "type")]` 映射回列名。

use crate::protocol::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
pub const DB_FILE_NAME: &str = "chat_histories.db";
pub const DEFAULT_TITLE: &str = "新会话";
pub const TASK_TYPE_INTERACTIVE: &str = "interactive";
pub const TITLE_SOURCE_DEFAULT: &str = "default";
pub const TITLE_SOURCE_FIRST_INPUT: &str = "first_input";
pub const TITLE_SOURCE_CUSTOM: &str = "custom";
pub const ENTRY_TYPE_CHANNEL_SELECTION: &str = "runtime/channel_selection";
pub const ENTRY_SUFFIX_CHANNEL_SELECTION: &str = ":channel-selection";
/// local_setting ownership (scope/scope_id are fixed to the single-user
/// layout; the namespace parameter keeps host namespaces isolated).
pub const SETTING_SCOPE: &str = "user";
pub const SETTING_SCOPE_ID: &str = "user";

// ---------- compaction constants (ZCode storage format vocabulary) ----------

/// ZCode part `type` value marking a compaction part.
pub const PART_TYPE_COMPACTION: &str = "compaction";
/// ZCode `semantics.kind` value marking a compact-summary message.
pub const SEMANTICS_KIND_COMPACT_SUMMARY: &str = "compact_summary";
/// Lead-in text ZCode writes ahead of the summary body in the text part
/// (`text = prefix + "\n\n" + body`); the read-side helper strips it.
pub const SUMMARY_CONTINUATION_PREFIX: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";
/// operationId prefix for host-initiated compactions (`cmp_` alone is the
/// ZCode engine prefix; `cmp_host_` marks host products; ZCode never reads
/// this store, so the extended prefix is safe).
pub const OPERATION_ID_HOST_PREFIX: &str = "cmp_host_";

// ---------- table models (column names = field names; epoch-millisecond times) ----------

/// channel extension table: project working directories.
#[derive(Debug, Clone, ormer::Model)]
#[table = "project"]
pub struct ProjectRow {
    #[primary]
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: i64,
}

/// ZCode 全列复刻；未用列（workspace_id 等）恒为 NULL。
#[derive(Debug, Clone, ormer::Model)]
#[table = "session"]
pub struct SessionRow {
    #[primary]
    pub id: String,
    #[index]
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub parent_id: Option<String>,
    pub slug: String,
    pub directory: String,
    pub path: Option<String>,
    pub title: String,
    pub version: String,
    pub share_url: Option<String>,
    pub summary_additions: Option<i64>,
    pub summary_deletions: Option<i64>,
    pub summary_files: Option<i64>,
    pub summary_diffs: Option<String>,
    pub revert: Option<String>,
    pub permission: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    /// 遗留列：现行 ZCode 从不写入，本模块同样不写（压缩不走 session 列，
    /// 压缩事实全在 compaction part 上）。
    pub time_compacting: Option<i64>,
    pub time_archived: Option<i64>,
    pub task_type: String,
    pub title_source: String,
    pub title_message_id: Option<String>,
    pub time_title_updated: Option<i64>,
    pub trace_id: Option<String>,
}

#[derive(Debug, Clone, ormer::Model)]
#[table = "message"]
pub struct MessageRow {
    #[primary]
    pub id: String,
    // 注意：模型不声明 #[foreign]。SQLite 无法给已有表补外键（ormer 会直接
    // 报 unmigratable schema），而共用库（~/chat_histories.db）里 ZCode 侧
    // 建的表自带外键——模型不声明时 ormer 保留存量外键不动。删除走显式
    // "先子后父"事务，不依赖外键级联。
    #[index]
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    /// ZCode 消息 JSON（键名照抄：sessionID / modelSelection / providerID…）。
    pub data: String,
    pub sequence: Option<i64>,
}

#[derive(Debug, Clone, ormer::Model)]
#[table = "part"]
pub struct PartRow {
    #[primary]
    pub id: String,
    // 不声明 #[foreign]，理由同 MessageRow；session_id 为冗余列（照抄 ZCode）。
    #[index]
    pub message_id: String,
    #[index]
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    /// `{"type":"text"|"reasoning","text":...,"time":{"start","end"}}`；
    /// 压缩 part 形状见 [`compaction_part_data`]。
    pub data: String,
    pub sequence: Option<i64>,
}

#[derive(Debug, Clone, ormer::Model)]
#[table = "session_entry"]
pub struct SessionEntryRow {
    #[primary]
    pub id: String,
    // 不声明 #[foreign]，理由同 MessageRow。
    #[index]
    pub session_id: String,
    /// `type` 是 Rust 关键字，字段改名 + 列名映射回 "type"。
    #[column(name = "type")]
    pub entry_type: String,
    pub time_created: i64,
    pub time_updated: i64,
    pub data: String,
}

#[derive(Debug, Clone, ormer::Model)]
#[table = "local_setting"]
pub struct LocalSettingRow {
    #[primary]
    pub scope: String,
    #[primary]
    pub scope_id: String,
    #[primary]
    pub namespace: String,
    #[primary]
    pub key: String,
    pub value: String,
    pub schema_version: i64,
    pub time_created: i64,
    pub time_updated: i64,
}

// ---------- records (high-level operation results) ----------

/// Project row projected for hosts (`created_at` widened to `u64` millis).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: u64,
}

/// Session row projected for hosts. `channel_key` comes from the session's
/// channel-selection entry (empty string when absent); its
/// `"{harness}|{team}|{model}"` semantics belong to the host — channel only
/// carries the string through.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub project_id: String,
    pub title: String,
    /// 稳定渠道 key（"{harness}|{team}|{model}"；语义属宿主，channel 仅透传）。
    pub channel_key: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// 存档时刻；None = 未存档（软隐藏标记，语义对齐 ZCode time.archived）。
    pub archived_at: Option<u64>,
}

/// Message row projected for hosts: content/reasoning merged from the
/// message's `text`/`reasoning` parts, role taken from the message `data`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    pub id: String,
    pub session_id: String,
    /// "user" | "assistant"（取自 message.data 的 role 键）。
    pub role: String,
    pub content: String,
    pub reasoning: String,
    pub timestamp: u64,
}

// ---------- ID generation (ZCode style) ----------

pub fn new_session_id() -> String {
    format!("sess_{}", uuid::Uuid::new_v4().hyphenated())
}

/// `msg_` / `part_` + 毫秒时间戳 base36 + uuid v4。
pub fn new_event_id(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        base36(now_millis() as u64),
        uuid::Uuid::new_v4().hyphenated()
    )
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while value > 0 {
        buf.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).expect("base36 output must be ASCII")
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 项目路径 slug：小写，[a-z0-9._-] 以外字符一律 '-'。
pub fn slugify(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

// ---------- data-column JSON (serde key names replicate ZCode exactly) ----------

#[derive(Debug, Serialize)]
struct MsgTime {
    created: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed: Option<i64>,
}

/// `modelSelection`（ZCode 现行键名 providerId/modelId）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSelection<'a> {
    provider_id: &'a str,
    model_id: &'a str,
}

/// 旧版 `model`（ZCode 遗留键名 providerID/modelID，大写 ID 原样保留）。
#[derive(Debug, Serialize)]
struct LegacyModelRef<'a> {
    #[serde(rename = "providerID")]
    provider_id: &'a str,
    #[serde(rename = "modelID")]
    model_id: &'a str,
}

#[derive(Debug, Serialize)]
struct UserMessageData<'a> {
    id: &'a str,
    #[serde(rename = "sessionID")]
    session_id: &'a str,
    role: &'static str,
    time: MsgTime,
    agent: &'a str,
    #[serde(rename = "modelSelection")]
    model_selection: ModelSelection<'a>,
    model: LegacyModelRef<'a>,
}

#[derive(Debug, Serialize)]
struct TokenCache {
    read: i64,
    write: i64,
}

#[derive(Debug, Serialize)]
struct Tokens {
    input: i64,
    output: i64,
    reasoning: i64,
    cache: TokenCache,
}

#[derive(Debug, Serialize)]
struct MsgPath<'a> {
    cwd: &'a str,
    root: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AssistantMessageData<'a> {
    id: &'a str,
    #[serde(rename = "sessionID")]
    session_id: &'a str,
    role: &'static str,
    time: MsgTime,
    #[serde(rename = "parentID")]
    parent_id: &'a str,
    model_id: &'a str,
    provider_id: &'a str,
    mode: &'static str,
    agent: &'a str,
    path: MsgPath<'a>,
    cost: i64,
    tokens: Tokens,
    finish: &'static str,
}

#[derive(Debug, Serialize, Deserialize)]
struct PartTime {
    start: i64,
    end: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PartData {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    time: PartTime,
}

#[derive(Debug, Serialize)]
struct ChannelSelectionData<'a> {
    #[serde(rename = "channelKey")]
    channel_key: &'a str,
}

pub fn user_message_data(
    msg_id: &str,
    session_id: &str,
    agent: &str,
    model: &str,
    created: i64,
) -> String {
    let data = UserMessageData {
        id: msg_id,
        session_id,
        role: "user",
        time: MsgTime {
            created,
            completed: None,
        },
        agent,
        model_selection: ModelSelection {
            provider_id: agent,
            model_id: model,
        },
        model: LegacyModelRef {
            provider_id: agent,
            model_id: model,
        },
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

#[allow(clippy::too_many_arguments)]
pub fn assistant_message_data(
    msg_id: &str,
    session_id: &str,
    parent_id: &str,
    agent: &str,
    model: &str,
    directory: &str,
    created: i64,
    completed: i64,
) -> String {
    let data = AssistantMessageData {
        id: msg_id,
        session_id,
        role: "assistant",
        time: MsgTime {
            created,
            completed: Some(completed),
        },
        parent_id,
        model_id: model,
        provider_id: agent,
        mode: "default",
        agent,
        path: MsgPath {
            cwd: directory,
            root: directory,
        },
        cost: 0,
        tokens: Tokens {
            input: 0,
            output: 0,
            reasoning: 0,
            cache: TokenCache { read: 0, write: 0 },
        },
        finish: "stop",
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

pub fn part_data(kind: &str, text: &str, start: i64, end: i64) -> String {
    let data = PartData {
        kind: kind.to_string(),
        text: text.to_string(),
        time: PartTime { start, end },
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

pub fn channel_selection_data(channel_key: &str) -> String {
    let data = ChannelSelectionData {
        channel_key,
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

// ---------- compaction types ----------

/// `preservedSegment` metadata: the kept tail of pre-boundary history that a
/// context rebuild re-inserts right after the summary message.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPreservedSegment {
    pub head_message_id: String,
    pub anchor_message_id: String,
    pub tail_message_id: String,
}

/// `compactBoundary` metadata; field names mirror the ZCode zod schema one
/// for one (serde camelCase). Fields ZCode marks optional are `Option` here
/// and skipped when `None`, so an in-row key set stays byte-shape compatible
/// with real ZCode rows and real engine payloads round-trip losslessly
/// (unknown extra keys are preserved via the flattened catch-all map).
/// A host filling this for its own compaction must populate: `boundary_id`,
/// `trigger`, `phase`, `compact_reason`, `pre_compact_token_count`,
/// `true_post_compact_token_count`, `tail_start_id`,
/// `summarized_message_count`, `kept_message_count` (the design's v1
/// required set; the other fields stay optional for engine passthrough).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionBoundary {
    /// `compact_` + uuid; the idempotency key of a compaction part.
    pub boundary_id: String,
    /// ZCode trigger enum: auto / manual / partial / reactive / session_memory.
    pub trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_source: Option<String>,
    pub pre_compact_token_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_compact_token_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_post_compact_token_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub will_retrigger_next_turn: Option<bool>,
    pub summarized_message_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_message_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_summarized_message_id: Option<String>,
    /// compaction part 的 `tail_start_id`（serde 键名 ZCode 原样，非驼峰；
    /// 被摘要区间末消息 id）。宿主自压必填；外部转写时引擎 boundary 载荷
    /// 不含该键（反序列化为 None），落库时 compaction_part_data 回落
    /// `last_summarized_message_id`（与 ZCode persistCompactSummary 的
    /// part 映射一致），序列化跳过 None 保持键组与引擎原样一致。
    #[serde(
        default,
        rename = "tail_start_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub tail_start_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<CompactionPreservedSegment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summary_message_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Catch-all keeping any payload key this struct does not model yet, so
    /// transcribed rows stay faithful to what the engine emitted.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The last active boundary of a session plus where it lives — the host's
/// material-selection anchor before triggering its own compaction, and the
/// UI's separator anchor.
#[derive(Clone, Debug)]
pub struct CompactionBoundaryView {
    pub boundary: CompactionBoundary,
    /// The summary message carrying the boundary compaction part.
    pub message_id: String,
    pub time_created: i64,
}

/// Everything [`ChatHistoryDatabase::insert_compaction_summary`] needs.
/// One transaction writes the summary message + text part + compaction part.
#[derive(Clone, Debug)]
pub struct CompactionSummaryParams {
    pub session_id: String,
    pub summary_title: String,
    pub summary_body: String,
    pub boundary: CompactionBoundary,
    /// `None` generates `"cmp_host_" + uuid` (host-product marker).
    pub operation_id: Option<String>,
    pub agent: String,
    pub model: String,
    /// `semantics.origin` source marker inside the identical format:
    /// `"blackagent"` for host self-compaction, `"zcode"` for external
    /// engine-compaction transcription (decision 12, 2026-10-16).
    pub origin: String,
}

#[derive(Debug, Serialize)]
struct CompactionSummaryData<'a> {
    id: &'a str,
    #[serde(rename = "sessionID")]
    session_id: &'a str,
    role: &'static str,
    time: MsgTime,
    summary: SummaryBody<'a>,
    agent: &'a str,
    #[serde(rename = "modelSelection")]
    model_selection: ModelSelection<'a>,
    semantics: CompactionSemantics<'a>,
    tools: Value,
}

#[derive(Debug, Serialize)]
struct SummaryBody<'a> {
    title: &'a str,
    body: &'a str,
    diffs: [u8; 0],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactionSemantics<'a> {
    origin: &'a str,
    kind: &'static str,
    ui_visibility: &'static str,
    provider_visibility: &'static str,
    transcript_visibility: &'static str,
}

/// summary message 的 data（ZCode 键名：role/time/summary/semantics/agent/
/// modelSelection/tools；语义对齐 ZCode persistCompactSummary 的落库形状）。
#[allow(clippy::too_many_arguments)]
pub fn compaction_summary_message_data(
    msg_id: &str,
    session_id: &str,
    agent: &str,
    model: &str,
    created: i64,
    title: &str,
    body: &str,
    origin: &str,
) -> String {
    let data = CompactionSummaryData {
        id: msg_id,
        session_id,
        role: "user",
        time: MsgTime {
            created,
            completed: None,
        },
        summary: SummaryBody {
            title,
            body,
            diffs: [],
        },
        agent,
        model_selection: ModelSelection {
            provider_id: agent,
            model_id: model,
        },
        semantics: CompactionSemantics {
            origin,
            kind: SEMANTICS_KIND_COMPACT_SUMMARY,
            ui_visibility: "hidden",
            provider_visibility: "visible",
            transcript_visibility: "hidden",
        },
        tools: serde_json::json!({}),
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactionPartData<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    auto: bool,
    trigger: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compact_reason: Option<&'a str>,
    #[serde(rename = "tail_start_id", skip_serializing_if = "Option::is_none")]
    tail_start_id: Option<&'a str>,
    compact_boundary: &'a CompactionBoundary,
    operation_id: &'a str,
}

/// compaction part 的 data（type/trigger/phase/compactReason/tail_start_id/
/// compactBoundary/operationId；键组与 ZCode 原生 compaction part 同构）。
pub fn compaction_part_data(boundary: &CompactionBoundary, operation_id: &str) -> String {
    let data = CompactionPartData {
        kind: PART_TYPE_COMPACTION,
        auto: boundary.trigger == "auto",
        trigger: &boundary.trigger,
        phase: boundary.phase.as_deref(),
        compact_reason: boundary.compact_reason.as_deref(),
        tail_start_id: boundary
            .tail_start_id
            .as_deref()
            .or(boundary.last_summarized_message_id.as_deref()),
        compact_boundary: boundary,
        operation_id,
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservedExternal {
    pre_tokens: u64,
    post_tokens: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactionMarkerPartData<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    auto: bool,
    trigger: &'static str,
    phase: &'static str,
    compact_reason: &'a str,
    observed_external: ObservedExternal,
    operation_id: String,
}

/// 外部压缩观察标记的 data（无 compactBoundary、带 observedExternal 扩展键）。
/// 观察标记永不参与切片：读侧只认带 `compactBoundary` 的 compaction part。
pub fn compaction_marker_part_data(reason: &str, pre_tokens: u64, post_tokens: u64) -> String {
    let data = CompactionMarkerPartData {
        kind: PART_TYPE_COMPACTION,
        auto: true,
        trigger: "reactive",
        phase: "mid_turn",
        compact_reason: reason,
        observed_external: ObservedExternal {
            pre_tokens,
            post_tokens,
        },
        operation_id: format!("{OPERATION_ID_HOST_PREFIX}{}", uuid::Uuid::new_v4()),
    };
    serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
}

/// The boundary carried by a compaction part, if the part is one.
fn compaction_boundary_of(part_data: &str) -> Option<CompactionBoundary> {
    let value: Value = serde_json::from_str(part_data).ok()?;
    if value.get("type").and_then(Value::as_str) != Some(PART_TYPE_COMPACTION) {
        return None;
    }
    serde_json::from_value(value.get("compactBoundary")?.clone()).ok()
}

/// Strip the ZCode continuation lead-in from a text-part summary body
/// (`text = prefix + "\n\n" + body`); texts without the prefix pass through.
fn strip_summary_continuation_prefix(text: &str) -> String {
    text.strip_prefix(SUMMARY_CONTINUATION_PREFIX)
        .and_then(|rest| rest.strip_prefix("\n\n"))
        .map(str::to_owned)
        .unwrap_or_else(|| text.to_owned())
}

// ---------- error mapping ----------

trait DatabaseResultExt<T> {
    fn db_error(self, message: &'static str) -> Result<T, Error>;
}

impl<T> DatabaseResultExt<T> for ormer::Result<T> {
    fn db_error(self, message: &'static str) -> Result<T, Error> {
        self.map_err(|error| Error::Backend(format!("{message}: {error}")))
    }
}

// ---------- handle ----------

/// Handle over the owned chat-history store. Internally a single ormer
/// (turso) connection behind a `tokio::sync::Mutex`: every operation —
/// including result-set consumption — runs under the lock, because two
/// overlapping queries on the same connection fail with
/// `concurrent use forbidden` (red line 4). Compound operations reuse the
/// `&Database` inside the lock instead of re-locking through `&self`; the
/// only escape hatch is [`ChatHistoryDatabase::lock`].
pub struct ChatHistoryDatabase {
    inner: tokio::sync::Mutex<ormer::Database>,
}

impl ChatHistoryDatabase {
    /// 库路径兜底解析：HOME/USERPROFILE + chat_histories.db。
    /// 【已拍板，2026-10-16】不走环境变量覆盖；存储位置由宿主经
    /// [`ChatHistoryDatabase::open_at`] 配置接口显式指定，本函数仅作
    /// 宿主未指定时的兜底。
    pub fn default_path() -> Result<PathBuf, Error> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .ok_or_else(|| {
                Error::Initialization("cannot locate the user home directory".to_owned())
            })?;
        Ok(PathBuf::from(home).join(DB_FILE_NAME))
    }

    /// 连兜底路径并完成初始化（等价 `open_at(&default_path()?)`）。
    pub async fn open() -> Result<Self, Error> {
        Self::open_at(&Self::default_path()?).await
    }

    /// 存储位置配置接口（2026-10-16 拍板）：宿主创建历史库句柄时调用，
    /// 指定库文件路径；此后该库内全部会话/消息内容均存储到该位置。
    /// 连接指定路径（库文件不存在则创建，自有库允许），依次：
    /// ① journal_mode=wal —— 查询通道（红线注释随迁：execute 遇结果行
    ///    报 unexpected row 属误报，副作用已生效；收 Vec<String> 校验
    ///    首行 == "wal"，非 wal 仅留痕不失败）；
    /// ② foreign_keys=on / busy_timeout=5000 —— execute 通道，失败留痕
    ///    不失败；
    /// ③ 六表迁移 migrate_schema（幂等，:memory: 之外的路径每次启动执行）。
    /// 打开/迁移失败返回 Err，由调用方决定去留。
    pub async fn open_at(path: &Path) -> Result<Self, Error> {
        let database = ormer::Database::connect(ormer::DbType::Sqlite, &path.to_string_lossy())
            .await
            .map_err(|error| {
                Error::Initialization(format!(
                    "failed to open chat history database {}: {error}",
                    path.display()
                ))
            })?;
        let this = Self {
            inner: tokio::sync::Mutex::new(database),
        };
        {
            let conn = this.inner.lock().await;
            // journal_mode 是查询型 PRAGMA（turso 声明无 NoColumns1，带参也返回
            // 一行当前模式），Connection::execute 遇到结果行必报 unexpected row；
            // 而切换副作用在产行之前已写盘生效。必须走查询路径消费结果行，以
            // 返回值判断是否生效，不能用 execute（只会误报未生效）。
            let modes: Vec<String> = match conn
                .select_sql::<String>(ormer::sql!("PRAGMA journal_mode=wal"))
                .collect::<Vec<String>>()
                .await
            {
                Ok(modes) => modes,
                Err(err) => {
                    eprintln!("[channel:history] setting journal_mode=wal failed (ignored): {err}");
                    Vec::new()
                }
            };
            if modes.first().map(String::as_str) != Some("wal") {
                // 非 wal 仍可运行（delete 模式可用），留痕即可，不得让启动失败。
                eprintln!("[channel:history] journal_mode is not wal (ignored): {modes:?}");
            }
            // busy_timeout / foreign_keys 带 RHS 参数时为零结果列，execute 正常。
            for pragma in ["PRAGMA foreign_keys=on", "PRAGMA busy_timeout=5000"] {
                if let Err(err) = conn.execute_sql(ormer::sql!(pragma)).await {
                    // turso 对部分 PRAGMA 可能不支持：记录后继续，不得让启动失败。
                    eprintln!("[channel:history] PRAGMA not applied (ignored): {pragma}: {err}");
                }
            }
            migrate_schema(&conn)
                .await
                .map_err(|error| Error::Initialization(error.to_string()))?;
        }
        Ok(this)
    }

    /// 高级/遗留导入专用：短暂独占底层 ormer::Database（事务、批量行插入）。
    ///
    /// # Contract
    ///
    /// The guard is single-operation scoped. While held, never call any other
    /// `&self` method of this handle: the tokio mutex is not re-entrant and
    /// nested acquisition deadlocks.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ormer::Database> {
        self.inner.lock().await
    }

    // ----- settings -----

    /// 读一个 namespace 下的全部设置键（scope/scope_id 固定 "user"）。
    /// 返回 key → JSON 值映射。
    pub async fn read_settings(
        &self,
        namespace: &str,
    ) -> Result<BTreeMap<String, Value>, Error> {
        read_settings(&*self.inner.lock().await, namespace).await
    }

    /// 写单个设置 key（存在则改 value/time_updated，不存在则插；先查后写，
    /// 不用 insert_or_update）。
    pub async fn write_local_setting(
        &self,
        namespace: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), Error> {
        write_local_setting(&*self.inner.lock().await, namespace, key, value).await
    }

    // ----- projects -----

    pub async fn list_projects(&self) -> Result<Vec<ProjectRecord>, Error> {
        list_projects(&*self.inner.lock().await).await
    }

    pub async fn find_project(&self, id: &str) -> Result<Option<ProjectRecord>, Error> {
        find_project(&*self.inner.lock().await, id).await
    }

    pub async fn project_path_exists(&self, path: &str) -> Result<bool, Error> {
        project_path_exists(&*self.inner.lock().await, path).await
    }

    pub async fn insert_project(&self, project: &ProjectRecord) -> Result<(), Error> {
        insert_project(&*self.inner.lock().await, project).await
    }

    /// 删项目：单事务先子后父（part/message/session_entry 按项目下全部
    /// session，再 session，再 project）。session_id 集合在事务内部自查，
    /// 返回给调用方（避免事务外查询的竞态窗口）。
    pub async fn delete_project_cascade(&self, project_id: &str) -> Result<Vec<String>, Error> {
        delete_project_cascade(&*self.inner.lock().await, project_id).await
    }

    // ----- sessions -----

    pub async fn insert_session(&self, row: &SessionRow) -> Result<(), Error> {
        insert_session(&*self.inner.lock().await, row).await
    }

    pub async fn find_session(&self, id: &str) -> Result<Option<SessionRow>, Error> {
        find_session(&*self.inner.lock().await, id).await
    }

    /// 不含已存档（time_archived IS NULL），updated_at 降序，channel_key 从
    /// session_entry 的 channel_selection join（缺失为空串）。
    pub async fn list_sessions(&self, project_id: &str) -> Result<Vec<SessionRecord>, Error> {
        list_sessions(&*self.inner.lock().await, project_id).await
    }

    pub async fn list_all_sessions(&self) -> Result<Vec<SessionRecord>, Error> {
        list_all_sessions(&*self.inner.lock().await).await
    }

    pub async fn list_session_ids(&self, project_id: &str) -> Result<Vec<String>, Error> {
        list_session_ids(&*self.inner.lock().await, project_id).await
    }

    /// title_source → custom，time_title_updated = now。会话不存在报错。
    pub async fn rename_session(&self, id: &str, title: &str) -> Result<(), Error> {
        rename_session(&*self.inner.lock().await, id, title).await
    }

    /// 存档 = time_archived = now（软隐藏，数据保留）；取消存档 = 置 NULL。
    pub async fn archive_session(&self, id: &str) -> Result<(), Error> {
        archive_session(&*self.inner.lock().await, id).await
    }

    pub async fn unarchive_session(&self, id: &str) -> Result<(), Error> {
        unarchive_session(&*self.inner.lock().await, id).await
    }

    /// time_updated 只前进不回退（MAX），原生 SQL。
    pub async fn touch_session(&self, id: &str, now: i64) -> Result<(), Error> {
        touch_session(&*self.inner.lock().await, id, now).await
    }

    /// title 仍为默认值时取输入前 24 字符；返回是否改过。
    pub async fn auto_title_if_default(&self, id: &str, text: &str) -> Result<bool, Error> {
        auto_title_if_default(&*self.inner.lock().await, id, text).await
    }

    /// 单事务先子后父：part → message → session_entry → session。
    pub async fn delete_session_cascade(&self, id: &str) -> Result<(), Error> {
        delete_session_cascade(&*self.inner.lock().await, id).await
    }

    // ----- channel selection (session_entry) -----

    pub async fn insert_channel_selection(
        &self,
        session_id: &str,
        channel_key: &str,
        now: i64,
    ) -> Result<(), Error> {
        insert_channel_selection(&*self.inner.lock().await, session_id, channel_key, now).await
    }

    /// entry 固定 id（"{session_id}:channel-selection"），存在改写、不存在新建。
    pub async fn update_channel_selection(
        &self,
        session_id: &str,
        channel_key: &str,
        now: i64,
    ) -> Result<(), Error> {
        update_channel_selection(&*self.inner.lock().await, session_id, channel_key, now).await
    }

    pub async fn find_channel_key(&self, session_id: &str) -> Result<Option<String>, Error> {
        find_channel_key(&*self.inner.lock().await, session_id).await
    }

    // ----- messages -----

    /// 落盘用户消息：message 行（data=user_message_data）+ 单 text part
    /// （sequence 0）。sequence 取该 session 当前 MAX+1。返回消息 id。
    pub async fn insert_user_message(
        &self,
        session_id: &str,
        agent: &str,
        model: &str,
        text: &str,
    ) -> Result<String, Error> {
        insert_user_message(&*self.inner.lock().await, session_id, agent, model, text).await
    }

    /// 落盘助手消息：message 行（data=assistant_message_data）+ parts
    /// （reasoning 非空时 reasoning=0、text=1，否则仅 text=0）。
    /// created=轮开始时间，completed=now。返回消息 id。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_assistant_message(
        &self,
        session_id: &str,
        parent_id: &str,
        agent: &str,
        model: &str,
        directory: &str,
        content: &str,
        reasoning: &str,
        created: i64,
        completed: i64,
    ) -> Result<String, Error> {
        insert_assistant_message(
            &*self.inner.lock().await,
            session_id,
            parent_id,
            agent,
            model,
            directory,
            content,
            reasoning,
            created,
            completed,
        )
        .await
    }

    /// 还原消息列表：content/reasoning 来自 part（按 kind 归并），role 来自
    /// message.data；排序 sequence asc（NULL 最后）→ time_created → id。
    /// 压缩 summary 行以 role=user + content=前导+正文 形式出现；compaction
    /// part 与观察标记不并入 content（§3.9.6 全量投影语义）。
    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<MessageRecord>, Error> {
        get_messages(&*self.inner.lock().await, session_id).await
    }

    // ----- compaction -----

    /// 落盘压缩摘要（单事务：summary message + text part + compaction part）。
    /// 幂等：同 boundary_id 已存在则跳过，返回 Ok(None)；成功返回消息 id。
    pub async fn insert_compaction_summary(
        &self,
        params: CompactionSummaryParams,
    ) -> Result<Option<String>, Error> {
        insert_compaction_summary(&*self.inner.lock().await, params).await
    }

    /// 外部压缩观察标记（挂在指定消息上；无 boundary，§3.9.5 兜底路径）。
    pub async fn record_external_compaction_marker(
        &self,
        session_id: &str,
        message_id: &str,
        reason: &str,
        pre_tokens: u64,
        post_tokens: u64,
    ) -> Result<(), Error> {
        record_external_compaction_marker(
            &*self.inner.lock().await,
            session_id,
            message_id,
            reason,
            pre_tokens,
            post_tokens,
        )
        .await
    }

    /// 读最后一个 active boundary（无则 None）——宿主触发压缩前判断
    /// "boundary 之后"选材区间用。
    pub async fn latest_compaction_boundary(
        &self,
        session_id: &str,
    ) -> Result<Option<CompactionBoundaryView>, Error> {
        latest_compaction_boundary(&*self.inner.lock().await, session_id).await
    }

    /// 切片还原（§3.9.6）：最后一个 active boundary（即 summary 消息）起
    /// 返回，preservedSegment 区间插回 summary 之后；无 boundary 时等价
    /// 全量。观察标记无 compactBoundary，任何情况下不影响切片。
    pub async fn get_context_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<MessageRecord>, Error> {
        get_context_messages(&*self.inner.lock().await, session_id).await
    }
}

// ---------- schema ----------

/// 六表 schema 对齐（幂等，open_at 每次执行）。宿主不需要在句柄外建表。
async fn migrate_schema(db: &ormer::Database) -> Result<(), Error> {
    db.migrate_table::<ProjectRow>()
        .await
        .db_error("failed to migrate the project table")?;
    db.migrate_table::<SessionRow>()
        .await
        .db_error("failed to migrate the session table")?;
    db.migrate_table::<MessageRow>()
        .await
        .db_error("failed to migrate the message table")?;
    db.migrate_table::<PartRow>()
        .await
        .db_error("failed to migrate the part table")?;
    db.migrate_table::<SessionEntryRow>()
        .await
        .db_error("failed to migrate the session_entry table")?;
    db.migrate_table::<LocalSettingRow>()
        .await
        .db_error("failed to migrate the local_setting table")?;
    Ok(())
}

// ---------- settings ----------

async fn read_settings(
    db: &ormer::Database,
    namespace: &str,
) -> Result<BTreeMap<String, Value>, Error> {
    let rows: Vec<LocalSettingRow> = db
        .select::<LocalSettingRow>()
        .filter(|m| m.scope.eq(SETTING_SCOPE.to_string()))
        .filter(|m| m.scope_id.eq(SETTING_SCOPE_ID.to_string()))
        .filter(|m| m.namespace.eq(namespace.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to read local settings")?;
    let mut settings = BTreeMap::new();
    for row in rows {
        match serde_json::from_str(&row.value) {
            Ok(value) => {
                settings.insert(row.key, value);
            }
            // 存量脏行不致命：跳过而不是让整个读取失败。
            Err(_) => continue,
        }
    }
    Ok(settings)
}

/// 写单个设置 key（先查后插或改，不用 insert_or_update）。
async fn write_local_setting(
    db: &ormer::Database,
    namespace: &str,
    key: &str,
    value: &Value,
) -> Result<(), Error> {
    let text = serde_json::to_string(value)
        .map_err(|error| Error::Backend(format!("failed to serialize the setting value: {error}")))?;
    let now = now_millis();
    let existing: Option<LocalSettingRow> = db
        .select::<LocalSettingRow>()
        .filter(|m| m.scope.eq(SETTING_SCOPE.to_string()))
        .filter(|m| m.scope_id.eq(SETTING_SCOPE_ID.to_string()))
        .filter(|m| m.namespace.eq(namespace.to_string()))
        .filter(|m| m.key.eq(key.to_string()))
        .first()
        .await
        .db_error("failed to query the local setting")?;
    if existing.is_some() {
        db.update::<LocalSettingRow>()
            .filter(|m| m.scope.eq(SETTING_SCOPE.to_string()))
            .filter(|m| m.scope_id.eq(SETTING_SCOPE_ID.to_string()))
            .filter(|m| m.namespace.eq(namespace.to_string()))
            .filter(|m| m.key.eq(key.to_string()))
            .set(|m| {
                m.value = m.value.set(text.clone());
                m.time_updated = m.time_updated.set(now);
            })
            .execute()
            .await
            .db_error("failed to update the local setting")?;
    } else {
        db.insert(&LocalSettingRow {
            scope: SETTING_SCOPE.to_string(),
            scope_id: SETTING_SCOPE_ID.to_string(),
            namespace: namespace.to_string(),
            key: key.to_string(),
            value: text,
            schema_version: 1,
            time_created: now,
            time_updated: now,
        })
        .execute()
        .await
        .db_error("failed to insert the local setting")?;
    }
    Ok(())
}

// ---------- projects ----------

fn project_record(row: ProjectRow) -> ProjectRecord {
    ProjectRecord {
        id: row.id,
        name: row.name,
        path: row.path,
        created_at: row.created_at as u64,
    }
}

async fn list_projects(db: &ormer::Database) -> Result<Vec<ProjectRecord>, Error> {
    let rows: Vec<ProjectRow> = db
        .select::<ProjectRow>()
        .order_by(|m| m.created_at.asc())
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query projects")?;
    Ok(rows.into_iter().map(project_record).collect())
}

async fn find_project(db: &ormer::Database, id: &str) -> Result<Option<ProjectRecord>, Error> {
    db.select::<ProjectRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .first()
        .await
        .db_error("failed to query the project")
        .map(|row| row.map(project_record))
}

async fn project_path_exists(db: &ormer::Database, path: &str) -> Result<bool, Error> {
    let found: Option<ProjectRow> = db
        .select::<ProjectRow>()
        .filter(|m| m.path.eq(path.to_string()))
        .first()
        .await
        .db_error("failed to query the project")?;
    Ok(found.is_some())
}

async fn insert_project(db: &ormer::Database, project: &ProjectRecord) -> Result<(), Error> {
    db.insert(&ProjectRow {
        id: project.id.clone(),
        name: project.name.clone(),
        path: project.path.clone(),
        created_at: project.created_at as i64,
    })
    .execute()
    .await
    .db_error("failed to insert the project")?;
    Ok(())
}

/// 删项目：一个事务里先子后父清掉全部关联行。session_id 集合在事务内自查，
/// 返回给调用方（运行态清理仍由宿主在调用前完成，见设计 §3.5）。
async fn delete_project_cascade(
    db: &ormer::Database,
    project_id: &str,
) -> Result<Vec<String>, Error> {
    let project_id = project_id.to_string();
    db.transaction(|txn| {
        let project_id = project_id.clone();
        Box::pin(async move {
            let ids: Vec<String> = txn
                .select::<SessionRow>()
                .filter(|m| m.project_id.eq(project_id.clone()))
                .collect::<Vec<_>>()
                .await?
                .into_iter()
                .map(|row| row.id)
                .collect();
            if !ids.is_empty() {
                txn.delete::<PartRow>()
                    .filter(|m| m.session_id.is_in(ids.clone()))
                    .execute()
                    .await?;
                txn.delete::<MessageRow>()
                    .filter(|m| m.session_id.is_in(ids.clone()))
                    .execute()
                    .await?;
                txn.delete::<SessionEntryRow>()
                    .filter(|m| m.session_id.is_in(ids.clone()))
                    .execute()
                    .await?;
            }
            txn.delete::<SessionRow>()
                .filter(|m| m.project_id.eq(project_id.clone()))
                .execute()
                .await?;
            txn.delete::<ProjectRow>()
                .filter(|m| m.id.eq(project_id.clone()))
                .execute()
                .await?;
            Ok(ids)
        })
    })
    .await
    .db_error("failed to delete the project")
}

// ---------- sessions ----------

async fn find_session(db: &ormer::Database, id: &str) -> Result<Option<SessionRow>, Error> {
    db.select::<SessionRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .first()
        .await
        .db_error("failed to query the session")
}

async fn insert_session(db: &ormer::Database, row: &SessionRow) -> Result<(), Error> {
    db.insert(row)
        .execute()
        .await
        .db_error("failed to insert the session")?;
    Ok(())
}

/// 从 channel_selection entry 的 data 列取 channelKey。
fn parse_channel_key(data: &str) -> Option<String> {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|v| v.get("channelKey").and_then(|k| k.as_str().map(str::to_owned)))
}

/// 渠道选择映射：session_id -> channelKey（全部 channel_selection entries）。
async fn channel_key_map(db: &ormer::Database) -> Result<HashMap<String, String>, Error> {
    let entries: Vec<SessionEntryRow> = db
        .select::<SessionEntryRow>()
        .filter(|m| m.entry_type.eq(ENTRY_TYPE_CHANNEL_SELECTION.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query channel selections")?;
    let mut map = HashMap::with_capacity(entries.len());
    for entry in entries {
        if let Some(key) = parse_channel_key(&entry.data) {
            map.insert(entry.session_id, key);
        }
    }
    Ok(map)
}

fn session_record(row: SessionRow, key_map: &HashMap<String, String>) -> SessionRecord {
    SessionRecord {
        channel_key: key_map.get(&row.id).cloned().unwrap_or_default(),
        id: row.id,
        project_id: row.project_id,
        title: row.title,
        created_at: row.time_created as u64,
        updated_at: row.time_updated as u64,
        archived_at: row.time_archived.map(|t| t as u64),
    }
}

async fn list_sessions(
    db: &ormer::Database,
    project_id: &str,
) -> Result<Vec<SessionRecord>, Error> {
    // 与 ZCode listSessions 一致：默认不含已存档（includeArchived=false）。
    let rows: Vec<SessionRow> = db
        .select::<SessionRow>()
        .filter(|m| m.project_id.eq(project_id.to_string()))
        .filter(|m| m.time_archived.is_null())
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query sessions")?;
    let key_map = channel_key_map(db).await?;
    let mut records: Vec<SessionRecord> =
        rows.into_iter().map(|r| session_record(r, &key_map)).collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    Ok(records)
}

async fn list_all_sessions(db: &ormer::Database) -> Result<Vec<SessionRecord>, Error> {
    let rows: Vec<SessionRow> = db
        .select::<SessionRow>()
        .filter(|m| m.time_archived.is_null())
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query sessions")?;
    let key_map = channel_key_map(db).await?;
    let mut records: Vec<SessionRecord> =
        rows.into_iter().map(|r| session_record(r, &key_map)).collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
    Ok(records)
}

async fn require_session(db: &ormer::Database, id: &str) -> Result<(), Error> {
    if find_session(db, id).await?.is_none() {
        return Err(Error::InvalidConfig(format!("session was not found: {id}")));
    }
    Ok(())
}

async fn rename_session(db: &ormer::Database, id: &str, title: &str) -> Result<(), Error> {
    require_session(db, id).await?;
    let now = now_millis();
    db.update::<SessionRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .set(|m| {
            m.title = m.title.set(title.to_string());
            m.title_source = m.title_source.set(TITLE_SOURCE_CUSTOM.to_string());
            m.time_title_updated = m.time_title_updated.set(Some(now));
        })
        .execute()
        .await
        .db_error("failed to rename the session")?;
    Ok(())
}

/// 存档会话：置 time_archived = now（软隐藏，数据保留），语义对齐 ZCode。
async fn archive_session(db: &ormer::Database, id: &str) -> Result<(), Error> {
    require_session(db, id).await?;
    db.update::<SessionRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .set(|m| m.time_archived = m.time_archived.set(Some(now_millis())))
        .execute()
        .await
        .db_error("failed to archive the session")?;
    Ok(())
}

/// 取消存档：清 time_archived，会话重新出现在列表。
async fn unarchive_session(db: &ormer::Database, id: &str) -> Result<(), Error> {
    require_session(db, id).await?;
    db.update::<SessionRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .set(|m| m.time_archived = m.time_archived.set(None::<i64>))
        .execute()
        .await
        .db_error("failed to unarchive the session")?;
    Ok(())
}

/// 删会话：一个事务里先子后父（part / message / session_entry / session），
/// 不依赖外键级联是否生效。
async fn delete_session_cascade(db: &ormer::Database, id: &str) -> Result<(), Error> {
    let id = id.to_string();
    db.transaction(|txn| {
        let id = id.clone();
        Box::pin(async move {
            txn.delete::<PartRow>()
                .filter(|m| m.session_id.eq(id.clone()))
                .execute()
                .await?;
            txn.delete::<MessageRow>()
                .filter(|m| m.session_id.eq(id.clone()))
                .execute()
                .await?;
            txn.delete::<SessionEntryRow>()
                .filter(|m| m.session_id.eq(id.clone()))
                .execute()
                .await?;
            txn.delete::<SessionRow>()
                .filter(|m| m.id.eq(id.clone()))
                .execute()
                .await?;
            Ok(())
        })
    })
    .await
    .db_error("failed to delete the session")
}

async fn list_session_ids(db: &ormer::Database, project_id: &str) -> Result<Vec<String>, Error> {
    let rows: Vec<SessionRow> = db
        .select::<SessionRow>()
        .filter(|m| m.project_id.eq(project_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query sessions")?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// 触碰会话：time_updated 只前进不回退。
async fn touch_session(db: &ormer::Database, id: &str, now: i64) -> Result<(), Error> {
    db.execute_sql(ormer::sql!(
        "UPDATE session SET time_updated = MAX(time_updated, {now}) WHERE id = {id}",
        now = now,
        id = id.to_string()
    ))
    .await
    .db_error("failed to touch the session")?;
    Ok(())
}

/// 首条消息自动标题：title 仍为默认值时取输入前 24 字符。
/// 返回是否改过标题。
async fn auto_title_if_default(db: &ormer::Database, id: &str, text: &str) -> Result<bool, Error> {
    let row = find_session(db, id)
        .await?
        .ok_or_else(|| Error::InvalidConfig(format!("session was not found: {id}")))?;
    if row.title != DEFAULT_TITLE {
        return Ok(false);
    }
    let brief: String = text.chars().take(24).collect();
    let now = now_millis();
    db.update::<SessionRow>()
        .filter(|m| m.id.eq(id.to_string()))
        .set(|m| {
            m.title = m.title.set(brief);
            m.title_source = m.title_source.set(TITLE_SOURCE_FIRST_INPUT.to_string());
            m.time_title_updated = m.time_title_updated.set(Some(now));
        })
        .execute()
        .await
        .db_error("failed to update the session title")?;
    Ok(true)
}

// ---------- channel selection (session_entry) ----------

async fn insert_channel_selection(
    db: &ormer::Database,
    session_id: &str,
    channel_key: &str,
    now: i64,
) -> Result<(), Error> {
    db.insert(&SessionEntryRow {
        id: format!("{session_id}{ENTRY_SUFFIX_CHANNEL_SELECTION}"),
        session_id: session_id.to_string(),
        entry_type: ENTRY_TYPE_CHANNEL_SELECTION.to_string(),
        time_created: now,
        time_updated: now,
        data: channel_selection_data(channel_key),
    })
    .execute()
    .await
    .db_error("failed to insert the channel selection")?;
    Ok(())
}

/// 更新会话的渠道选择（entry 固定 id，存在则改写，不存在则新建）。
async fn update_channel_selection(
    db: &ormer::Database,
    session_id: &str,
    channel_key: &str,
    now: i64,
) -> Result<(), Error> {
    let updated = db
        .update::<SessionEntryRow>()
        .filter(|m| m.id.eq(format!("{session_id}{ENTRY_SUFFIX_CHANNEL_SELECTION}")))
        .set(|m| {
            m.data = m.data.set(channel_selection_data(channel_key));
            m.time_updated = m.time_updated.set(now);
        })
        .execute()
        .await
        .db_error("failed to update the channel selection")?;
    if updated == 0 {
        return insert_channel_selection(db, session_id, channel_key, now).await;
    }
    Ok(())
}

/// 单会话的渠道 key（session_entry 的 channel_selection）。
async fn find_channel_key(
    db: &ormer::Database,
    session_id: &str,
) -> Result<Option<String>, Error> {
    let rows: Option<SessionEntryRow> = db
        .select::<SessionEntryRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .filter(|m| m.entry_type.eq(ENTRY_TYPE_CHANNEL_SELECTION.to_string()))
        .first()
        .await
        .db_error("failed to query the channel selection")?;
    Ok(rows.and_then(|row| parse_channel_key(&row.data)))
}

// ---------- messages ----------

/// 该 session 的下一条 message.sequence（空表从 0 开始）。
async fn next_message_sequence(db: &ormer::Database, session_id: &str) -> Result<i64, Error> {
    let rows: Vec<i64> = db
        .select_sql::<i64>(ormer::sql!(
            "SELECT COALESCE(MAX(sequence), -1) FROM message WHERE session_id = {sid}",
            sid = session_id.to_string()
        ))
        .collect::<Vec<i64>>()
        .await
        .db_error("failed to query the next message sequence")?;
    Ok(rows.into_iter().next().unwrap_or(-1) + 1)
}

async fn insert_message(db: &ormer::Database, row: &MessageRow) -> Result<(), Error> {
    db.insert(row)
        .execute()
        .await
        .db_error("failed to insert the message")?;
    Ok(())
}

async fn insert_part(db: &ormer::Database, row: &PartRow) -> Result<(), Error> {
    db.insert(row)
        .execute()
        .await
        .db_error("failed to insert the message part")?;
    Ok(())
}

/// 落盘用户消息：message 行 + 单个 text part（sequence 0）。
/// 返回消息 id。
async fn insert_user_message(
    db: &ormer::Database,
    session_id: &str,
    agent: &str,
    model: &str,
    text: &str,
) -> Result<String, Error> {
    let now = now_millis();
    let sequence = next_message_sequence(db, session_id).await?;
    let id = new_event_id("msg");
    insert_message(
        db,
        &MessageRow {
            id: id.clone(),
            session_id: session_id.to_string(),
            time_created: now,
            time_updated: now,
            data: user_message_data(&id, session_id, agent, model, now),
            sequence: Some(sequence),
        },
    )
    .await?;
    insert_part(
        db,
        &PartRow {
            id: new_event_id("part"),
            message_id: id.clone(),
            session_id: session_id.to_string(),
            time_created: now,
            time_updated: now,
            data: part_data("text", text, now, now),
            sequence: Some(0),
        },
    )
    .await?;
    Ok(id)
}

/// 落盘助手消息：message 行 + parts（reasoning 非空时 reasoning=0、text=1，
/// 否则仅 text=0）。created=轮开始时间，completed=now。
#[allow(clippy::too_many_arguments)]
async fn insert_assistant_message(
    db: &ormer::Database,
    session_id: &str,
    parent_id: &str,
    agent: &str,
    model: &str,
    directory: &str,
    content: &str,
    reasoning: &str,
    created: i64,
    completed: i64,
) -> Result<String, Error> {
    let sequence = next_message_sequence(db, session_id).await?;
    let id = new_event_id("msg");
    insert_message(
        db,
        &MessageRow {
            id: id.clone(),
            session_id: session_id.to_string(),
            time_created: created,
            time_updated: completed,
            data: assistant_message_data(
                &id, session_id, parent_id, agent, model, directory, created, completed,
            ),
            sequence: Some(sequence),
        },
    )
    .await?;
    let text_sequence = if reasoning.is_empty() { 0 } else { 1 };
    if !reasoning.is_empty() {
        insert_part(
            db,
            &PartRow {
                id: new_event_id("part"),
                message_id: id.clone(),
                session_id: session_id.to_string(),
                time_created: created,
                time_updated: completed,
                data: part_data("reasoning", reasoning, created, completed),
                sequence: Some(0),
            },
        )
        .await?;
    }
    insert_part(
        db,
        &PartRow {
            id: new_event_id("part"),
            message_id: id.clone(),
            session_id: session_id.to_string(),
            time_created: created,
            time_updated: completed,
            data: part_data("text", content, created, completed),
            sequence: Some(text_sequence),
        },
    )
    .await?;
    Ok(id)
}

/// 消息排序：sequence asc（NULL 最后）→ time_created → id。
fn sort_messages(messages: &mut [MessageRow]) {
    messages.sort_by(|a, b| {
        a.sequence
            .unwrap_or(i64::MAX)
            .cmp(&b.sequence.unwrap_or(i64::MAX))
            .then(a.time_created.cmp(&b.time_created))
            .then(a.id.cmp(&b.id))
    });
}

/// part 排序：message_id → sequence asc（NULL 最后）→ id。
fn sort_parts(parts: &mut [PartRow]) {
    parts.sort_by(|a, b| {
        a.message_id
            .cmp(&b.message_id)
            .then(
                a.sequence
                    .unwrap_or(i64::MAX)
                    .cmp(&b.sequence.unwrap_or(i64::MAX)),
            )
            .then(a.id.cmp(&b.id))
    });
}

/// part 归并：text/reasoning 各并入对应桶；其余 kind（compaction、timeline
/// 等 ZCode 投影 part）不进消息正文；非 JSON 的存量脏 part 按原文当 text
/// （对齐迁移来源的兜底行为）。
fn merge_parts_into_buckets(
    parts: &[PartRow],
    text_by_message: &mut HashMap<String, String>,
    reasoning_by_message: &mut HashMap<String, String>,
) {
    for part in parts {
        match serde_json::from_str::<Value>(&part.data) {
            Ok(value) => {
                let kind = value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("text");
                let text = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let target = if kind == "reasoning" {
                    &mut *reasoning_by_message
                } else if kind == "text" {
                    &mut *text_by_message
                } else {
                    continue;
                };
                target
                    .entry(part.message_id.clone())
                    .or_default()
                    .push_str(text);
            }
            Err(_) => {
                text_by_message
                    .entry(part.message_id.clone())
                    .or_default()
                    .push_str(&part.data);
            }
        }
    }
}

async fn load_message_records(
    db: &ormer::Database,
    session_id: &str,
) -> Result<Vec<MessageRecord>, Error> {
    let mut messages: Vec<MessageRow> = db
        .select::<MessageRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query messages")?;
    sort_messages(&mut messages);
    let mut parts: Vec<PartRow> = db
        .select::<PartRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query message parts")?;
    sort_parts(&mut parts);
    // 归并桶：reasoning 与 text 分开；各自 part 顺序即写入顺序。
    let mut text_by_message: HashMap<String, String> = HashMap::new();
    let mut reasoning_by_message: HashMap<String, String> = HashMap::new();
    merge_parts_into_buckets(&parts, &mut text_by_message, &mut reasoning_by_message);
    Ok(messages
        .into_iter()
        .map(|m| {
            let role = serde_json::from_str::<Value>(&m.data)
                .ok()
                .and_then(|v| v.get("role").and_then(|r| r.as_str().map(str::to_owned)))
                .unwrap_or_else(|| "user".into());
            let mid = m.id.clone();
            MessageRecord {
                content: text_by_message.remove(&mid).unwrap_or_default(),
                reasoning: reasoning_by_message.remove(&mid).unwrap_or_default(),
                id: m.id,
                session_id: m.session_id,
                role,
                timestamp: m.time_created as u64,
            }
        })
        .collect())
}

async fn get_messages(db: &ormer::Database, session_id: &str) -> Result<Vec<MessageRecord>, Error> {
    load_message_records(db, session_id).await
}

// ---------- compaction ----------

/// The text part attached to a compaction summary message
/// (`{"type":"text","synthetic":true,"text":prefix + "\n\n" + body,...}`).
#[derive(Debug, Serialize)]
struct CompactionTextPartData {
    #[serde(rename = "type")]
    kind: &'static str,
    synthetic: bool,
    text: String,
    time: PartTime,
}

fn compaction_text_part_text(body: &str) -> String {
    format!("{SUMMARY_CONTINUATION_PREFIX}\n\n{body}")
}

/// 落盘压缩摘要：单事务写 summary message + text part + compaction part。
/// 幂等：同 session 内已存在同 boundaryId 的 compaction part 时跳过并返回
/// None（重复转写/重试安全）。
async fn insert_compaction_summary(
    db: &ormer::Database,
    params: CompactionSummaryParams,
) -> Result<Option<String>, Error> {
    let session_id = params.session_id.clone();
    let boundary = params.boundary.clone();
    let boundary_id = boundary.boundary_id.clone();
    let title = params.summary_title;
    let body = params.summary_body;
    let agent = params.agent;
    let model = params.model;
    let origin = params.origin;
    let operation_id = params
        .operation_id
        .unwrap_or_else(|| format!("{OPERATION_ID_HOST_PREFIX}{}", uuid::Uuid::new_v4()));
    db.transaction(|txn| {
        let session_id = session_id.clone();
        let boundary_id = boundary_id.clone();
        let boundary = boundary.clone();
        let title = title.clone();
        let body = body.clone();
        let agent = agent.clone();
        let model = model.clone();
        let origin = origin.clone();
        let operation_id = operation_id.clone();
        Box::pin(async move {
            // 幂等键：同 session 内任一 compaction part 已携带同 boundaryId。
            let parts: Vec<PartRow> = txn
                .select::<PartRow>()
                .filter(|m| m.session_id.eq(session_id.clone()))
                .collect::<Vec<_>>()
                .await?;
            if parts
                .iter()
                .any(|part| {
                    compaction_boundary_of(&part.data)
                        .map(|existing| existing.boundary_id == boundary_id)
                        .unwrap_or(false)
                })
            {
                return Ok(None);
            }
            // sequence 取当前 MAX+1（事务内自查，锁内无并发窗口）。
            let messages: Vec<MessageRow> = txn
                .select::<MessageRow>()
                .filter(|m| m.session_id.eq(session_id.clone()))
                .collect::<Vec<_>>()
                .await?;
            let sequence = messages
                .iter()
                .filter_map(|m| m.sequence)
                .max()
                .map(|max| max + 1)
                .unwrap_or(0);
            let now = now_millis();
            let message_id = new_event_id("msg");
            txn.insert(&MessageRow {
                id: message_id.clone(),
                session_id: session_id.clone(),
                time_created: now,
                time_updated: now,
                data: compaction_summary_message_data(
                    &message_id, &session_id, &agent, &model, now, &title, &body, &origin,
                ),
                sequence: Some(sequence),
            })
            .execute()
            .await?;
            txn.insert(&PartRow {
                id: new_event_id("part"),
                message_id: message_id.clone(),
                session_id: session_id.clone(),
                time_created: now,
                time_updated: now,
                data: serde_json::to_string(&CompactionTextPartData {
                    kind: "text",
                    synthetic: true,
                    text: compaction_text_part_text(&body),
                    time: PartTime { start: now, end: now },
                })
                .map_err(|error| {
                    ormer::ormer_error!("failed to serialize the compaction text part: {error}")
                })?,
                sequence: Some(0),
            })
            .execute()
            .await?;
            txn.insert(&PartRow {
                id: new_event_id("part"),
                message_id: message_id.clone(),
                session_id: session_id.clone(),
                time_created: now,
                time_updated: now,
                data: compaction_part_data(&boundary, &operation_id),
                sequence: None,
            })
            .execute()
            .await?;
            Ok(Some(message_id))
        })
    })
    .await
    .db_error("failed to insert the compaction summary")
}

/// 外部压缩观察标记：挂在指定消息上的无 boundary compaction part。
async fn record_external_compaction_marker(
    db: &ormer::Database,
    session_id: &str,
    message_id: &str,
    reason: &str,
    pre_tokens: u64,
    post_tokens: u64,
) -> Result<(), Error> {
    let now = now_millis();
    insert_part(
        db,
        &PartRow {
            id: new_event_id("part"),
            message_id: message_id.to_string(),
            session_id: session_id.to_string(),
            time_created: now,
            time_updated: now,
            data: compaction_marker_part_data(reason, pre_tokens, post_tokens),
            sequence: None,
        },
    )
    .await
}

async fn latest_compaction_boundary(
    db: &ormer::Database,
    session_id: &str,
) -> Result<Option<CompactionBoundaryView>, Error> {
    let mut messages: Vec<MessageRow> = db
        .select::<MessageRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query messages")?;
    sort_messages(&mut messages);
    let parts: Vec<PartRow> = db
        .select::<PartRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query message parts")?;
    // 从后向前找最后一条挂有 active boundary（带 compactBoundary 的
    // compaction part）的消息；旧 boundary 自然失活。
    for message in messages.iter().rev() {
        for part in parts.iter().filter(|part| part.message_id == message.id) {
            if let Some(boundary) = compaction_boundary_of(&part.data) {
                return Ok(Some(CompactionBoundaryView {
                    boundary,
                    message_id: message.id.clone(),
                    time_created: message.time_created,
                }));
            }
        }
    }
    Ok(None)
}

async fn get_context_messages(
    db: &ormer::Database,
    session_id: &str,
) -> Result<Vec<MessageRecord>, Error> {
    let mut messages: Vec<MessageRow> = db
        .select::<MessageRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query messages")?;
    sort_messages(&mut messages);
    let parts: Vec<PartRow> = db
        .select::<PartRow>()
        .filter(|m| m.session_id.eq(session_id.to_string()))
        .collect::<Vec<_>>()
        .await
        .db_error("failed to query message parts")?;
    // ① 最后一个 active boundary（边界消息即 summary 消息本身）。
    let mut boundary_index: Option<usize> = None;
    let mut boundary: Option<CompactionBoundary> = None;
    for (index, message) in messages.iter().enumerate().rev() {
        for part in parts.iter().filter(|part| part.message_id == message.id) {
            if let Some(found) = compaction_boundary_of(&part.data) {
                boundary_index = Some(index);
                boundary = Some(found);
                break;
            }
        }
        if boundary_index.is_some() {
            break;
        }
    }
    let Some(index) = boundary_index else {
        // 无 boundary：切片等价全量。
        return load_message_records(db, session_id).await;
    };
    let mut picked: Vec<MessageRow> = Vec::new();
    // ② summary 消息本身保留为第一条 user 消息。
    picked.push(messages[index].clone());
    // ③ preservedSegment（head~tail 闭区间）插回 summary 之后、其余之前。
    if let Some(segment) = boundary.as_ref().and_then(|b| b.preserved_segment.clone()) {
        let head = messages[..index]
            .iter()
            .position(|m| m.id == segment.head_message_id);
        let tail = messages[..index]
            .iter()
            .rposition(|m| m.id == segment.tail_message_id);
        if let (Some(head), Some(tail)) = (head, tail) {
            if head <= tail {
                picked.extend_from_slice(&messages[head..=tail]);
            }
        }
    }
    // boundary 之后（不含 boundary 消息）的全部消息。
    picked.extend_from_slice(&messages[index + 1..]);
    // 复用全量投影的归并逻辑（compaction/timeline part 不进正文）。
    let mut text_by_message: HashMap<String, String> = HashMap::new();
    let mut reasoning_by_message: HashMap<String, String> = HashMap::new();
    merge_parts_into_buckets(&parts, &mut text_by_message, &mut reasoning_by_message);
    Ok(picked
        .into_iter()
        .map(|m| {
            let role = serde_json::from_str::<Value>(&m.data)
                .ok()
                .and_then(|v| v.get("role").and_then(|r| r.as_str().map(str::to_owned)))
                .unwrap_or_else(|| "user".into());
            let mid = m.id.clone();
            MessageRecord {
                content: text_by_message.remove(&mid).unwrap_or_default(),
                reasoning: reasoning_by_message.remove(&mid).unwrap_or_default(),
                id: m.id,
                session_id: m.session_id,
                role,
                timestamp: m.time_created as u64,
            }
        })
        .collect())
}

// ---------- external compaction transcription helpers ----------

/// ZCode 引擎库路径：HOME/.zcode/cli/db/db.sqlite；环境变量
/// CHANNEL_ZCODE_SESSION_DB 覆盖（测试与异机部署用；读外部库的场景
/// 与"自有库路径不走环境变量"的拍板不冲突，后者只约束 chat_histories.db）。
pub fn zcode_session_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CHANNEL_ZCODE_SESSION_DB") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".zcode").join("cli").join("db").join("db.sqlite")
}

/// 按 summaryMessageId 读引擎摘要正文。优先 message.data 的
/// summary.body（纯正文）；缺失或解析失败回落该消息 text part 的
/// text 剥除 SUMMARY_CONTINUATION_PREFIX；两者皆无返回 Ok(None)。
/// rusqlite 只读连接（SQLITE_OPEN_READ_ONLY），打开/查询失败返回
/// Err（宿主降级占位转写，不 panic）；引擎进程仍在写时 WAL 只读
/// 快照安全。库文件不存在返回 Ok(None)。
pub async fn read_zcode_compaction_summary_text(
    session_id: &str,
    message_id: &str,
) -> Result<Option<String>, Error> {
    let path = zcode_session_db_path();
    if !path.is_file() {
        return Ok(None);
    }
    let connection = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| {
        Error::Initialization(format!(
            "failed to open the ZCode session database {}: {error}",
            path.display()
        ))
    })?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT data FROM message WHERE id = ?1 AND session_id = ?2",
            rusqlite::params![message_id, session_id],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            error => Err(error),
        })
        .map_err(|error| {
            Error::Backend(format!(
                "failed to query the ZCode session database: {error}"
            ))
        })?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    if let Ok(value) = serde_json::from_str::<Value>(&raw) {
        if let Some(body) = value.pointer("/summary/body").and_then(Value::as_str) {
            return Ok(Some(body.to_owned()));
        }
    }
    // 回落：该消息的 text part（引擎库形状：type=="text" 携带前导+正文）。
    let mut statement = connection
        .prepare(
            "SELECT data FROM part WHERE message_id = ?1 AND session_id = ?2 ORDER BY sequence",
        )
        .map_err(|error| {
            Error::Backend(format!(
                "failed to query the ZCode session database: {error}"
            ))
        })?;
    let mut rows = statement
        .query(rusqlite::params![message_id, session_id])
        .map_err(|error| {
            Error::Backend(format!(
                "failed to query the ZCode session database: {error}"
            ))
        })?;
    while let Some(row) = rows
        .next()
        .map_err(|error| {
            Error::Backend(format!(
                "failed to read the ZCode session database row: {error}"
            ))
        })?
    {
        let data: String = row
            .get(0)
            .map_err(|error| {
                Error::Backend(format!(
                    "failed to read the ZCode session database row: {error}"
                ))
            })?;
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                return Ok(Some(strip_summary_continuation_prefix(text)));
            }
        }
    }
    Ok(None)
}

// ---------- tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base36_encodes_zero_and_radix_boundaries() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(9), "9");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(123456789), "21i3v9");
    }

    #[test]
    fn slugify_lowercases_and_replaces_unsafe_characters() {
        assert_eq!(slugify("/tmp/Demo Project"), "-tmp-demo-project");
        assert_eq!(slugify("a_b.c-d"), "a_b.c-d");
        assert_eq!(slugify("中文目录"), "----");
        assert_eq!(slugify(""), "-");
    }

    #[test]
    fn id_shapes_match_the_zcode_contract() {
        let session_id = new_session_id();
        assert!(session_id.starts_with("sess_"));
        let uuid = session_id.trim_start_matches("sess_");
        assert_eq!(uuid.len(), 36, "hyphenated uuid v4 has 36 characters");
        assert_eq!(uuid.matches('-').count(), 4);

        let event_id = new_event_id("msg");
        assert!(event_id.starts_with("msg_"));
        let rest = event_id.trim_start_matches("msg_");
        let mut parts = rest.split('_');
        let stamp = parts.next().unwrap_or_default();
        let uuid = parts.next().unwrap_or_default();
        assert!(!stamp.is_empty(), "base36 millisecond stamp present");
        assert!(stamp.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
        assert_eq!(uuid.len(), 36);
    }

    #[test]
    fn user_message_data_keys_replicate_zcode() {
        let data: Value = serde_json::from_str(&user_message_data("msg_1", "sess_1", "ZCode", "GLM", 1))
            .unwrap();
        let keys: Vec<&str> = data.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["agent", "id", "model", "modelSelection", "role", "sessionID", "time"]
        );
        assert_eq!(data["sessionID"], "sess_1");
        assert_eq!(data["role"], "user");
        assert_eq!(data["modelSelection"]["providerId"], "ZCode");
        assert_eq!(data["modelSelection"]["modelId"], "GLM");
        assert_eq!(data["model"]["providerID"], "ZCode");
        assert_eq!(data["model"]["modelID"], "GLM");
        assert_eq!(data["time"]["created"], 1);
        assert!(data["time"].get("completed").is_none());
    }

    #[test]
    fn assistant_message_data_keys_replicate_zcode() {
        let data: Value = serde_json::from_str(&assistant_message_data(
            "msg_2", "sess_1", "msg_1", "ZCode", "GLM", "/tmp", 1, 2,
        ))
        .unwrap();
        assert_eq!(data["role"], "assistant");
        assert_eq!(data["parentID"], "msg_1");
        assert_eq!(data["modelId"], "GLM");
        assert_eq!(data["providerId"], "ZCode");
        assert_eq!(data["path"]["cwd"], "/tmp");
        assert_eq!(data["tokens"]["cache"]["read"], 0);
        assert_eq!(data["finish"], "stop");
        assert!(data.get("mode").is_some());
        assert_eq!(data["time"]["completed"], 2);
    }

    #[test]
    fn part_and_channel_selection_data_keys_replicate_zcode() {
        let part: Value = serde_json::from_str(&part_data("text", "hi", 1, 2)).unwrap();
        assert_eq!(part["type"], "text");
        assert_eq!(part["text"], "hi");
        assert_eq!(part["time"]["start"], 1);
        assert_eq!(part["time"]["end"], 2);

        let entry: Value = serde_json::from_str(&channel_selection_data("ZCode|t|m")).unwrap();
        assert_eq!(entry["channelKey"], "ZCode|t|m");
    }

    #[test]
    fn compaction_data_shapes_match_the_zcode_native_rows() {
        let boundary = CompactionBoundary {
            boundary_id: "compact_abc".to_owned(),
            trigger: "auto".to_owned(),
            phase: Some("mid_turn".to_owned()),
            compact_reason: Some("context_limit".to_owned()),
            summary_source: None,
            pre_compact_token_count: 9000,
            post_compact_token_count: Some(1200),
            true_post_compact_token_count: Some(1300),
            auto_compact_threshold: Some(150000),
            will_retrigger_next_turn: Some(false),
            summarized_message_count: 8,
            kept_message_count: Some(1),
            last_summarized_message_id: Some("msg_tail".to_owned()),
            tail_start_id: Some("msg_tail".to_owned()),
            preserved_segment: Some(CompactionPreservedSegment {
                head_message_id: "msg_head".to_owned(),
                anchor_message_id: "msg_anchor".to_owned(),
                tail_message_id: "msg_tail".to_owned(),
            }),
            summary_message_ids: vec!["msg_sum".to_owned()],
            custom_instructions: None,
            trace_id: Some("trace_1".to_owned()),
            turn_id: Some("turn_1".to_owned()),
            extra: BTreeMap::new(),
        };
        let message: Value = serde_json::from_str(&compaction_summary_message_data(
            "msg_sum", "sess_1", "zcode-agent", "GLM", 7, "Compact summary", "正文", "zcode",
        ))
        .unwrap();
        assert_eq!(message["role"], "user");
        assert_eq!(message["sessionID"], "sess_1");
        assert_eq!(message["summary"]["title"], "Compact summary");
        assert_eq!(message["summary"]["body"], "正文");
        assert!(message["summary"]["diffs"].is_array());
        assert_eq!(message["semantics"]["origin"], "zcode");
        assert_eq!(message["semantics"]["kind"], "compact_summary");
        assert_eq!(message["semantics"]["uiVisibility"], "hidden");
        assert_eq!(message["semantics"]["providerVisibility"], "visible");
        assert_eq!(message["semantics"]["transcriptVisibility"], "hidden");
        assert_eq!(message["modelSelection"]["providerId"], "zcode-agent");
        assert!(message["tools"].is_object());

        let part: Value =
            serde_json::from_str(&compaction_part_data(&boundary, "cmp_1")).unwrap();
        assert_eq!(part["type"], "compaction");
        assert_eq!(part["auto"], true);
        assert_eq!(part["trigger"], "auto");
        assert_eq!(part["phase"], "mid_turn");
        assert_eq!(part["compactReason"], "context_limit");
        assert_eq!(part["tail_start_id"], "msg_tail");
        assert_eq!(part["compactBoundary"]["boundaryId"], "compact_abc");
        assert_eq!(part["operationId"], "cmp_1");

        let marker: Value =
            serde_json::from_str(&compaction_marker_part_data("usage_drop", 8000, 2000)).unwrap();
        assert_eq!(marker["type"], "compaction");
        assert!(marker.get("compactBoundary").is_none());
        assert_eq!(marker["observedExternal"]["preTokens"], 8000);
        assert_eq!(marker["observedExternal"]["postTokens"], 2000);
        assert!(marker["operationId"]
            .as_str()
            .unwrap()
            .starts_with(OPERATION_ID_HOST_PREFIX));
    }

    #[test]
    fn compaction_boundary_round_trips_a_realistic_payload() {
        let payload = serde_json::json!({
            "boundaryId": "compact_0b1c",
            "trigger": "auto",
            "phase": "mid_turn",
            "compactReason": "context_limit",
            "summarySource": "model",
            "preCompactTokenCount": 155000,
            "postCompactTokenCount": 21000,
            "truePostCompactTokenCount": 22400,
            "autoCompactThreshold": 166000,
            "willRetriggerNextTurn": false,
            "summarizedMessageCount": 42,
            "keptMessageCount": 2,
            "lastSummarizedMessageId": "msg_41",
            "preservedSegment": {
                "headMessageId": "msg_40",
                "anchorMessageId": "msg_40",
                "tailMessageId": "msg_41"
            },
            "summaryMessageIds": ["msg_42"],
            "customInstructions": false,
            "traceId": "tr_1",
            "turnId": "turn_9",
            "futureEngineField": {"nested": true}
        });
        let boundary: CompactionBoundary = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(boundary.boundary_id, "compact_0b1c");
        assert_eq!(boundary.tail_start_id, None);
        let round: Value = serde_json::to_value(&boundary).unwrap();
        assert_eq!(round, payload, "round trip must be lossless");
    }

    #[test]
    fn summary_continuation_prefix_is_stripped() {
        let text = format!("{SUMMARY_CONTINUATION_PREFIX}\n\n正文内容");
        assert_eq!(strip_summary_continuation_prefix(&text), "正文内容");
        assert_eq!(
            strip_summary_continuation_prefix("没有前导的正文"),
            "没有前导的正文"
        );
    }
}
