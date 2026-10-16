//! history 压缩（compaction）语义测试：自压落库形状与键名快照、幂等、
//! 切片语义、观察标记、外部转写（真实 boundary payload fixture）、
//! ZCode 引擎库正文只读辅助（rusqlite fixture，经 CHANNEL_ZCODE_SESSION_DB
//! 注入）。全部 :memory:/临时文件，不碰真实 `~/chat_histories.db` 与
//! `~/.zcode`。
//!
//! 运行：cargo test --features history --test history_compaction

#![cfg(feature = "history")]

use std::path::Path;
use std::sync::Arc;

use channel::{
    ChatHistoryDatabase, CompactionBoundary, CompactionPreservedSegment, CompactionSummaryParams,
    ProjectRecord, SessionRow, SUMMARY_CONTINUATION_PREFIX, TASK_TYPE_INTERACTIVE,
    TITLE_SOURCE_DEFAULT, new_session_id, slugify,
};

async fn fresh_db() -> ChatHistoryDatabase {
    ChatHistoryDatabase::open_at(Path::new(":memory:"))
        .await
        .expect("打开 :memory: 失败")
}

fn make_boundary(id: &str, tail: Option<&str>) -> CompactionBoundary {
    CompactionBoundary {
        boundary_id: id.to_owned(),
        trigger: "auto".to_owned(),
        phase: Some("mid_turn".to_owned()),
        compact_reason: Some("context_limit".to_owned()),
        summary_source: None,
        pre_compact_token_count: 9000,
        post_compact_token_count: Some(1200),
        true_post_compact_token_count: Some(1300),
        auto_compact_threshold: Some(150000),
        will_retrigger_next_turn: Some(false),
        summarized_message_count: 2,
        kept_message_count: Some(1),
        last_summarized_message_id: Some("msg_last".to_owned()),
        tail_start_id: tail.map(str::to_owned),
        preserved_segment: Some(CompactionPreservedSegment {
            head_message_id: String::new(),
            anchor_message_id: String::new(),
            tail_message_id: String::new(),
        }),
        summary_message_ids: vec![],
        custom_instructions: None,
        trace_id: Some("trace_1".to_owned()),
        turn_id: None,
        extra: Default::default(),
    }
}

async fn seed(db: &ChatHistoryDatabase, turns: usize) -> String {
    db.insert_project(&ProjectRecord {
        id: "prj_1".to_owned(),
        name: "demo".to_owned(),
        path: "/tmp/demo".to_owned(),
        created_at: 1,
    })
    .await
    .unwrap();
    let sid = new_session_id();
    db.insert_session(&SessionRow {
        id: sid.clone(),
        project_id: "prj_1".to_owned(),
        workspace_id: None,
        parent_id: None,
        slug: slugify("/tmp/demo"),
        directory: "/tmp/demo".to_owned(),
        path: None,
        title: "会话".to_owned(),
        version: "0.1.0".to_owned(),
        share_url: None,
        summary_additions: None,
        summary_deletions: None,
        summary_files: None,
        summary_diffs: None,
        revert: None,
        permission: None,
        time_created: 1,
        time_updated: 1,
        time_compacting: None,
        time_archived: None,
        task_type: TASK_TYPE_INTERACTIVE.to_owned(),
        title_source: TITLE_SOURCE_DEFAULT.to_owned(),
        title_message_id: None,
        time_title_updated: None,
        trace_id: None,
    })
    .await
    .unwrap();
    let mut last_user = String::new();
    for round in 0..turns {
        let user = db
            .insert_user_message(&sid, "ZCode", "GLM", &format!("问{round}"))
            .await
            .unwrap();
        db.insert_assistant_message(
            &sid,
            &user,
            "ZCode",
            "GLM",
            "/tmp/demo",
            &format!("答{round}"),
            "",
            100 + round as i64,
            200 + round as i64,
        )
        .await
        .unwrap();
        last_user = user;
    }
    let _ = last_user;
    sid
}

async fn counts(db: &ChatHistoryDatabase) -> (i64, i64) {
    let conn = db.lock().await;
    let messages = conn
        .select_sql::<i64>(ormer::sql!("SELECT COUNT(*) FROM message"))
        .collect::<Vec<i64>>()
        .await
        .unwrap()[0];
    let parts = conn
        .select_sql::<i64>(ormer::sql!("SELECT COUNT(*) FROM part"))
        .collect::<Vec<i64>>()
        .await
        .unwrap()[0];
    (messages, parts)
}

fn params<'a>(
    session_id: &str,
    boundary: &'a CompactionBoundary,
    title: &'a str,
    body: &'a str,
) -> CompactionSummaryParams {
    CompactionSummaryParams {
        session_id: session_id.to_owned(),
        summary_title: title.to_owned(),
        summary_body: body.to_owned(),
        boundary: boundary.clone(),
        operation_id: None,
        agent: "blackagent".to_owned(),
        model: "GLM".to_owned(),
        origin: "blackagent".to_owned(),
    }
}

// ---------- 1. 自压落库：三行形状与键名快照（对齐 ZCode 原生样本） ----------

#[tokio::test]
async fn self_compaction_persists_summary_message_with_two_parts() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let (messages_before, parts_before) = counts(&db).await;

    let boundary = make_boundary("compact_self_1", Some("msg_tail"));
    let message_id = db
        .insert_compaction_summary(params(&sid, &boundary, "Compact summary", "摘要正文"))
        .await
        .unwrap()
        .expect("首次落库应返回消息 id");

    let (messages_after, parts_after) = counts(&db).await;
    assert_eq!(messages_after, messages_before + 1, "新增一条 summary message");
    assert_eq!(parts_after, parts_before + 2, "新增 text part + compaction part");

    // 行形状与键名快照
    let conn = db.lock().await;
    let row: channel::MessageRow = conn
        .select::<channel::MessageRow>()
        .filter(|m| m.id.eq(message_id.clone()))
        .first()
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    let data: serde_json::Value = serde_json::from_str(&row.data).unwrap();
    assert_eq!(data["role"], "user");
    assert_eq!(data["summary"]["title"], "Compact summary");
    assert_eq!(data["summary"]["body"], "摘要正文");
    assert_eq!(data["semantics"]["origin"], "blackagent");
    assert_eq!(data["semantics"]["kind"], "compact_summary");
    assert_eq!(data["semantics"]["uiVisibility"], "hidden");
    assert_eq!(data["semantics"]["providerVisibility"], "visible");
    assert_eq!(data["semantics"]["transcriptVisibility"], "hidden");

    let conn = db.lock().await;
    let parts: Vec<channel::PartRow> = conn
        .select::<channel::PartRow>()
        .filter(|m| m.message_id.eq(message_id.clone()))
        .collect::<Vec<_>>()
        .await
        .unwrap();
    drop(conn);
    assert_eq!(parts.len(), 2);
    let text_part = parts.iter().find(|p| p.data.contains("\"text\"")).unwrap();
    let text: serde_json::Value = serde_json::from_str(&text_part.data).unwrap();
    assert_eq!(
        text["text"],
        format!("{SUMMARY_CONTINUATION_PREFIX}\n\n摘要正文")
    );
    assert_eq!(text["synthetic"], true);
    let compaction_part = parts
        .iter()
        .find(|p| p.data.contains("\"compaction\""))
        .unwrap();
    let compaction: serde_json::Value = serde_json::from_str(&compaction_part.data).unwrap();
    assert_eq!(compaction["type"], "compaction");
    assert_eq!(compaction["auto"], true);
    assert_eq!(compaction["trigger"], "auto");
    assert_eq!(compaction["phase"], "mid_turn");
    assert_eq!(compaction["compactReason"], "context_limit");
    assert_eq!(compaction["tail_start_id"], "msg_tail");
    assert_eq!(compaction["compactBoundary"]["boundaryId"], "compact_self_1");
    assert!(compaction["operationId"]
        .as_str()
        .unwrap()
        .starts_with("cmp_host_"));

    // latest_compaction_boundary 指向该 summary 消息
    let view = db.latest_compaction_boundary(&sid).await.unwrap().unwrap();
    assert_eq!(view.boundary.boundary_id, "compact_self_1");
    assert_eq!(view.message_id, message_id);
}

// ---------- 2. 幂等：同 boundaryId 二次落库跳过 ----------

#[tokio::test]
async fn insert_is_idempotent_per_boundary_id() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let boundary = make_boundary("compact_idem", Some("msg_tail"));

    let first = db
        .insert_compaction_summary(params(&sid, &boundary, "Compact summary", "正文"))
        .await
        .unwrap();
    assert!(first.is_some());
    let (messages, parts) = counts(&db).await;

    // 同 boundaryId 重试：跳过（重复转写/重试安全）
    let second = db
        .insert_compaction_summary(params(&sid, &boundary, "Compact summary", "正文"))
        .await
        .unwrap();
    assert_eq!(second, None, "同 boundaryId 二次落库应跳过");
    assert_eq!(counts(&db).await, (messages, parts), "行数不变");

    // 不同 boundaryId 正常落库
    let other = make_boundary("compact_idem_2", Some("msg_tail"));
    let third = db
        .insert_compaction_summary(params(&sid, &other, "Compact summary", "正文二"))
        .await
        .unwrap();
    assert!(third.is_some());
    assert_eq!(counts(&db).await, (messages + 1, parts + 2));

    // 多 boundary 只认最后一个
    let view = db.latest_compaction_boundary(&sid).await.unwrap().unwrap();
    assert_eq!(view.boundary.boundary_id, "compact_idem_2");
}

// ---------- 3. 切片语义 ----------

#[tokio::test]
async fn context_slice_without_boundary_is_full() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let all = db.get_messages(&sid).await.unwrap();
    let context = db.get_context_messages(&sid).await.unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(
        context.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        all.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
        "无 boundary 时切片等价全量"
    );
}

#[tokio::test]
async fn context_slice_keeps_summary_and_reinserts_preserved_segment() {
    let db = fresh_db().await;
    let sid = seed(&db, 3).await;
    let all = db.get_messages(&sid).await.unwrap();
    // 消息序：u0 a0 u1 a1 u2 a2；保留最后一轮（u2, a2）为 preservedSegment。
    let u2 = all[4].id.clone();
    let a2 = all[5].id.clone();

    let mut boundary = make_boundary("compact_slice_1", Some(&a2));
    boundary.preserved_segment = Some(CompactionPreservedSegment {
        head_message_id: u2.clone(),
        anchor_message_id: u2.clone(),
        tail_message_id: a2.clone(),
    });
    boundary.summarized_message_count = 4;
    boundary.kept_message_count = Some(2);
    let summary_id = db
        .insert_compaction_summary(params(&sid, &boundary, "Compact summary", "四轮已摘要"))
        .await
        .unwrap()
        .unwrap();

    let context = db.get_context_messages(&sid).await.unwrap();
    let ids: Vec<&str> = context.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![summary_id.as_str(), u2.as_str(), a2.as_str()],
        "summary 在首、preservedSegment 插回其后、boundary 前老消息剔除"
    );
    // summary 消息投影为 user 角色 + 前导正文
    assert_eq!(context[0].role, "user");
    assert!(context[0].content.starts_with(SUMMARY_CONTINUATION_PREFIX));
    assert!(context[0].content.ends_with("四轮已摘要"));
}

#[tokio::test]
async fn context_slice_only_honors_the_last_boundary() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let all = db.get_messages(&sid).await.unwrap();
    let a0 = all[1].id.clone();
    let a1 = all[3].id.clone();

    // 第一个 boundary：保留 a0
    let mut first = make_boundary("compact_old", Some(&a0));
    first.preserved_segment = Some(CompactionPreservedSegment {
        head_message_id: a0.clone(),
        anchor_message_id: a0.clone(),
        tail_message_id: a0.clone(),
    });
    let first_summary = db
        .insert_compaction_summary(params(&sid, &first, "Compact summary", "第一轮摘要"))
        .await
        .unwrap()
        .unwrap();
    // 第二个 boundary：保留 a1
    let mut second = make_boundary("compact_new", Some(&a1));
    second.preserved_segment = Some(CompactionPreservedSegment {
        head_message_id: a1.clone(),
        anchor_message_id: a1.clone(),
        tail_message_id: a1.clone(),
    });
    let second_summary = db
        .insert_compaction_summary(params(&sid, &second, "Compact summary", "第二轮摘要"))
        .await
        .unwrap()
        .unwrap();

    let context = db.get_context_messages(&sid).await.unwrap();
    let ids: Vec<&str> = context.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![second_summary.as_str(), a1.as_str()],
        "旧 boundary 失活，只保留最后一个 boundary 的切片"
    );
    assert!(!ids.contains(&first_summary.as_str()));
}

// ---------- 4. 观察标记不参与切片 ----------

#[tokio::test]
async fn observation_marker_never_affects_the_slice() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let all = db.get_messages(&sid).await.unwrap();
    let last = all[3].id.clone();

    // 无 boundary，只有观察标记：切片仍等价全量
    db.record_external_compaction_marker(&sid, &last, "usage_drop", 8000, 2000)
        .await
        .unwrap();
    assert!(db.latest_compaction_boundary(&sid).await.unwrap().is_none());
    let context = db.get_context_messages(&sid).await.unwrap();
    assert_eq!(context.len(), 4);
    // 标记 part 不进消息正文
    assert!(context.iter().all(|m| !m.content.contains("compaction")));

    // 有 boundary 之后追加观察标记：切片仍只认 boundary
    let mut boundary = make_boundary("compact_marker", Some(&last));
    boundary.preserved_segment = None;
    let summary_id = db
        .insert_compaction_summary(params(&sid, &boundary, "Compact summary", "正文"))
        .await
        .unwrap()
        .unwrap();
    db.record_external_compaction_marker(&sid, &last, "usage_drop", 7000, 2100)
        .await
        .unwrap();
    let context = db.get_context_messages(&sid).await.unwrap();
    let ids: Vec<&str> = context.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec![summary_id.as_str()]);
}

// ---------- 5. 级联删除随消息行清理压缩行 ----------

#[tokio::test]
async fn cascade_delete_removes_compaction_rows() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;
    let mut boundary = make_boundary("compact_cascade", None);
    boundary.preserved_segment = None;
    db.insert_compaction_summary(params(&sid, &boundary, "Compact summary", "正文"))
        .await
        .unwrap()
        .unwrap();
    assert!(counts(&db).await.1 >= 4);
    db.delete_session_cascade(&sid).await.unwrap();
    assert_eq!(counts(&db).await, (0, 0), "summary message 与两个 part 一并清理");
}

// ---------- 6. 外部转写：真实 stream-json boundary payload ----------

/// stream-json `session.updated` 行的 `payload` 实拍形状（字段名与 ZCode
/// contracts 的 compactBoundaryPayloadSchema 一一对应；含一个本结构未建模
/// 的引擎扩展键，验证 flatten 兜底无损）。
const REAL_SESSION_UPDATED_PAYLOAD: &str = r#"{
    "boundaryId": "compact_7f3a2c",
    "trigger": "auto",
    "phase": "mid_turn",
    "compactReason": "context_limit",
    "summarySource": "model",
    "preCompactTokenCount": 155320,
    "postCompactTokenCount": 21330,
    "truePostCompactTokenCount": 22470,
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
    "traceId": "trace_9f2",
    "turnId": "turn_77",
    "spanId": "span_1"
}"#;

#[tokio::test]
async fn external_transcription_round_trips_the_engine_payload() {
    let db = fresh_db().await;
    let sid = seed(&db, 2).await;

    // 宿主侧转写入口：payload 原样 JSON → CompactionBoundary（serde camelCase
    // 与 zod 键组一一对应）。
    let payload: serde_json::Value = serde_json::from_str(REAL_SESSION_UPDATED_PAYLOAD).unwrap();
    let boundary: CompactionBoundary = serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(boundary.summary_message_ids, vec!["msg_42".to_owned()]);
    assert_eq!(boundary.last_summarized_message_id.as_deref(), Some("msg_41"));

    let params = CompactionSummaryParams {
        session_id: sid.clone(),
        summary_title: "Compact summary".to_owned(),
        summary_body: "引擎摘要正文（如实取自引擎库）".to_owned(),
        boundary,
        operation_id: Some("cmp_7f3a2c".to_owned()),
        agent: "zcode-agent".to_owned(),
        model: "GLM".to_owned(),
        origin: "zcode".to_owned(),
    };
    let message_id = db.insert_compaction_summary(params).await.unwrap().unwrap();

    // 行键组与 ZCode 原生样本一致：compactBoundary 反序列化往返无损
    //（含未建模的 spanId 扩展键），tail_start_id 回落 lastSummarizedMessageId
    //（与 persistCompactSummary 的 part 映射一致）。
    let conn = db.lock().await;
    let parts: Vec<channel::PartRow> = conn
        .select::<channel::PartRow>()
        .filter(|m| m.message_id.eq(message_id.clone()))
        .collect::<Vec<_>>()
        .await
        .unwrap();
    drop(conn);
    let compaction_part = parts
        .iter()
        .find(|p| p.data.contains("\"compaction\""))
        .unwrap();
    let compaction: serde_json::Value = serde_json::from_str(&compaction_part.data).unwrap();
    assert_eq!(compaction["compactBoundary"], payload, "boundary 往返无损");
    assert_eq!(compaction["tail_start_id"], "msg_41");
    assert_eq!(compaction["operationId"], "cmp_7f3a2c");

    // summary message 的 origin 如实写 zcode
    let conn = db.lock().await;
    let row: channel::MessageRow = conn
        .select::<channel::MessageRow>()
        .filter(|m| m.id.eq(message_id.clone()))
        .first()
        .await
        .unwrap()
        .unwrap();
    drop(conn);
    let data: serde_json::Value = serde_json::from_str(&row.data).unwrap();
    assert_eq!(data["semantics"]["origin"], "zcode");
}

// ---------- 7. 压缩写与并行读不 forbidden ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_writes_survive_concurrent_reads() {
    let db = Arc::new(fresh_db().await);
    let sid = seed(&db, 2).await;
    let mut set = tokio::task::JoinSet::new();
    for worker in 0..6usize {
        let db = db.clone();
        let sid = sid.clone();
        set.spawn(async move {
            for round in 0..20u32 {
                if worker % 2 == 0 {
                    // 写侧：逐轮落不同的压缩 boundary
                    let boundary = channel::CompactionBoundary {
                        boundary_id: format!("compact_w{worker}r{round}"),
                        trigger: "auto".to_owned(),
                        phase: Some("mid_turn".to_owned()),
                        compact_reason: Some("context_limit".to_owned()),
                        summary_source: None,
                        pre_compact_token_count: 1000 + round as u64,
                        post_compact_token_count: None,
                        true_post_compact_token_count: Some(300),
                        auto_compact_threshold: None,
                        will_retrigger_next_turn: None,
                        summarized_message_count: 1,
                        kept_message_count: None,
                        last_summarized_message_id: None,
                        tail_start_id: None,
                        preserved_segment: None,
                        summary_message_ids: vec![],
                        custom_instructions: None,
                        trace_id: Some("trace".to_owned()),
                        turn_id: None,
                        extra: Default::default(),
                    };
                    db.insert_compaction_summary(CompactionSummaryParams {
                        session_id: sid.clone(),
                        summary_title: "Compact summary".to_owned(),
                        summary_body: "并发写正文".to_owned(),
                        boundary,
                        operation_id: None,
                        agent: "blackagent".to_owned(),
                        model: "GLM".to_owned(),
                        origin: "blackagent".to_owned(),
                    })
                    .await
                    .expect("并发压缩写失败");
                } else {
                    // 读侧：全量投影 + 切片 + boundary 查询（结果集消费全程在锁内）
                    db.get_messages(&sid).await.expect("并发读消息失败");
                    db.get_context_messages(&sid).await.expect("并发切片失败");
                    db.latest_compaction_boundary(&sid)
                        .await
                        .expect("并发读 boundary 失败");
                }
            }
        });
    }
    while let Some(done) = set.join_next().await {
        done.expect("任务 panic");
    }
    // 写侧 3 worker × 20 轮全部落库成功（boundaryId 各不相同）
    let view = db.latest_compaction_boundary(&sid).await.unwrap();
    assert!(view.is_some());
}

// ---------- 8. ZCode 引擎库正文只读辅助 ----------

/// ENGINE_DB_ENV 串行化 env 注入（env 是进程级全局，测试并行跑会互踩）。
static ENGINE_DB_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 用 rusqlite 建真实引擎库形状的 fixture：message.data 的 summary.body /
/// text part 前导剥离 / 查无此行 None / 库不存在 None。
mod engine_fixture {
    use rusqlite::Connection;
    use std::path::Path;

    pub fn create(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("建 fixture 库失败");
        connection
            .execute_batch(
                "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT, sequence INTEGER);",
            )
            .unwrap();
        connection
    }

    pub fn insert_message(connection: &Connection, id: &str, session_id: &str, data: &str) {
        connection
            .execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, session_id, data],
            )
            .unwrap();
    }

    pub fn insert_part(
        connection: &Connection,
        id: &str,
        message_id: &str,
        session_id: &str,
        data: &str,
        sequence: i64,
    ) {
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, data, sequence) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, message_id, session_id, data, sequence],
            )
            .unwrap();
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn engine_summary_body_is_preferred_and_text_part_falls_back() {
    let _guard = ENGINE_DB_ENV.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db.sqlite");
    let connection = engine_fixture::create(&db_path);

    // 消息 1：data 带 summary.body（纯正文）+ text part（前导+正文）→ 优先 body
    engine_fixture::insert_message(
        &connection,
        "msg_1",
        "sess_1",
        r#"{"role":"user","summary":{"title":"Compact summary","body":"纯摘要正文"}}"#,
    );
    engine_fixture::insert_part(
        &connection,
        "part_1",
        "msg_1",
        "sess_1",
        &serde_json::json!({
            "type": "text",
            "synthetic": true,
            "text": format!("{SUMMARY_CONTINUATION_PREFIX}\n\n纯摘要正文")
        })
        .to_string(),
        0,
    );
    // 消息 2：data 无 summary（解析回落）→ text part 剥前导
    engine_fixture::insert_message(&connection, "msg_2", "sess_1", r#"{"role":"user"}"#);
    engine_fixture::insert_part(
        &connection,
        "part_2",
        "msg_2",
        "sess_1",
        &serde_json::json!({
            "type": "text",
            "text": format!("{SUMMARY_CONTINUATION_PREFIX}\n\n回落正文")
        })
        .to_string(),
        0,
    );
    drop(connection);

    std::env::set_var("CHANNEL_ZCODE_SESSION_DB", &db_path);
    let body = block_on(channel::read_zcode_compaction_summary_text("sess_1", "msg_1"))
        .unwrap()
        .unwrap();
    assert_eq!(body, "纯摘要正文", "优先 message.data 的 summary.body");
    let body = block_on(channel::read_zcode_compaction_summary_text("sess_1", "msg_2"))
        .unwrap()
        .unwrap();
    assert_eq!(body, "回落正文", "回落 text part 并剥除前导文案");
    // 查无此行 → Ok(None)
    let none = block_on(channel::read_zcode_compaction_summary_text("sess_1", "msg_none"))
        .unwrap();
    assert_eq!(none, None);
    std::env::remove_var("CHANNEL_ZCODE_SESSION_DB");
}

#[test]
fn engine_db_missing_file_returns_none() {
    let _guard = ENGINE_DB_ENV.lock().unwrap();
    // 库文件不存在 → Ok(None)
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sqlite");
    std::env::set_var("CHANNEL_ZCODE_SESSION_DB", &missing);
    let none = block_on(channel::read_zcode_compaction_summary_text("sess_1", "msg_1"))
        .unwrap();
    assert_eq!(none, None);
    std::env::remove_var("CHANNEL_ZCODE_SESSION_DB");
}
