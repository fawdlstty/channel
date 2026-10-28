//! history feature 功能语义测试：全部跑 :memory:，不落盘、不碰真实
//! `~/chat_histories.db` 与 `~/.zcode`。
//!
//! 语义迁移自 BlackAgent `src-tauri/tests/db.rs`（实现迁移到 channel 后
//! 的对应回归），设置域改为 namespace 参数化语义。
//!
//! 运行：cargo test --features history --test history

#![cfg(feature = "history")]

use std::path::Path;

use channel::{
    ChatHistoryDatabase, DB_FILE_NAME, DEFAULT_TITLE, ProjectRecord, SessionRow,
    TASK_TYPE_INTERACTIVE, TITLE_SOURCE_CUSTOM, TITLE_SOURCE_DEFAULT, TITLE_SOURCE_FIRST_INPUT,
    new_session_id, slugify,
};

async fn fresh_db() -> ChatHistoryDatabase {
    ChatHistoryDatabase::open_at(Path::new(":memory:"))
        .await
        .expect("打开 :memory: 失败")
}

/// 造一个 session 行 + 渠道选择 entry，返回 session id。
async fn seed_session(
    db: &ChatHistoryDatabase,
    project_id: &str,
    directory: &str,
    channel_key: &str,
    time_created: i64,
    time_updated: i64,
) -> String {
    let id = new_session_id();
    db.insert_session(&SessionRow {
        id: id.clone(),
        project_id: project_id.to_string(),
        workspace_id: None,
        parent_id: None,
        slug: slugify(directory),
        directory: directory.to_string(),
        path: None,
        title: DEFAULT_TITLE.into(),
        version: "0.1.0".into(),
        share_url: None,
        summary_additions: None,
        summary_deletions: None,
        summary_files: None,
        summary_diffs: None,
        revert: None,
        permission: None,
        time_created,
        time_updated,
        time_compacting: None,
        time_archived: None,
        task_type: TASK_TYPE_INTERACTIVE.into(),
        title_source: TITLE_SOURCE_DEFAULT.into(),
        title_message_id: None,
        time_title_updated: None,
        trace_id: None,
    })
    .await
    .expect("插入 session 失败");
    db.insert_channel_selection(&id, channel_key, time_created)
        .await
        .expect("插入渠道选择失败");
    id
}

async fn count(db: &ChatHistoryDatabase, table: &str) -> i64 {
    let conn = db.lock().await;
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.select_sql::<i64>(ormer::sql!(sql))
        .collect::<Vec<i64>>()
        .await
        .expect("count 失败")
        .into_iter()
        .next()
        .unwrap_or(0)
}

fn project(id: &str) -> ProjectRecord {
    ProjectRecord {
        id: id.to_owned(),
        name: "demo".to_owned(),
        path: "/tmp/demo".to_owned(),
        created_at: 1000,
    }
}

// ---------- 1. 会话往返 ----------

#[tokio::test]
async fn message_roundtrip_content_reasoning_order() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let sid = seed_session(&db, "prj_1", "/tmp/demo", "ZCode|glm|GLM-5.3", 1, 1).await;

    // 第一轮：user → assistant（带 reasoning）
    let user1 = db
        .insert_user_message(&sid, "ZCode", "GLM-5.3", "第一问")
        .await
        .unwrap();
    let assistant1 = db
        .insert_assistant_message(
            &sid, &user1, "ZCode", "GLM-5.3", "/tmp/demo", "第一答", "第一推理", 2000, 2500,
        )
        .await
        .unwrap();
    // 第二轮：user → assistant（无 reasoning）
    let user2 = db
        .insert_user_message(&sid, "ZCode", "GLM-5.3", "第二问")
        .await
        .unwrap();
    db.insert_assistant_message(
        &sid, &user2, "ZCode", "GLM-5.3", "/tmp/demo", "第二答", "", 3000, 3500,
    )
    .await
    .unwrap();

    let messages = db.get_messages(&sid).await.unwrap();
    assert_eq!(messages.len(), 4, "应有 4 条消息");
    // 顺序：user1 → assistant1 → user2 → assistant2
    assert_eq!(messages[0].id, user1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "第一问");
    assert_eq!(messages[0].reasoning, "");
    assert_eq!(messages[1].id, assistant1);
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "第一答");
    assert_eq!(messages[1].reasoning, "第一推理");
    assert_eq!(messages[1].timestamp, 2000);
    assert_eq!(messages[2].content, "第二问");
    assert_eq!(messages[3].content, "第二答");
    assert_eq!(messages[3].reasoning, "");

    // data 列 JSON 键名符合 ZCode 格式
    let conn = db.lock().await;
    let rows: Vec<channel::MessageRow> = conn
        .select::<channel::MessageRow>()
        .filter(|m| m.session_id.eq(sid.clone()))
        .collect::<Vec<_>>()
        .await
        .unwrap();
    drop(conn);
    let user_row = rows.iter().find(|m| m.id == user1).unwrap();
    let data: serde_json::Value = serde_json::from_str(&user_row.data).unwrap();
    assert_eq!(data["sessionID"], sid.as_str(), "sessionID 大写 D");
    assert_eq!(data["role"], "user");
    assert_eq!(data["agent"], "ZCode");
    assert_eq!(data["modelSelection"]["providerId"], "ZCode");
    assert_eq!(data["modelSelection"]["modelId"], "GLM-5.3");
    assert_eq!(data["model"]["providerID"], "ZCode", "providerID 大写 ID");
    assert_eq!(data["model"]["modelID"], "GLM-5.3");
    let assistant_row = rows.iter().find(|m| m.id == assistant1).unwrap();
    let adata: serde_json::Value = serde_json::from_str(&assistant_row.data).unwrap();
    assert_eq!(adata["role"], "assistant");
    assert_eq!(adata["parentID"], user1.as_str());
    assert_eq!(adata["modelId"], "GLM-5.3");
    assert_eq!(adata["providerId"], "ZCode");
    assert_eq!(adata["path"]["cwd"], "/tmp/demo");
    assert_eq!(adata["tokens"]["cache"]["read"], 0);
    assert_eq!(adata["finish"], "stop");
    assert!(adata.get("mode").is_some());
    // part 格式
    let conn = db.lock().await;
    let parts: Vec<channel::PartRow> = conn
        .select::<channel::PartRow>()
        .filter(|m| m.session_id.eq(sid.clone()))
        .collect::<Vec<_>>()
        .await
        .unwrap();
    drop(conn);
    assert_eq!(parts.len(), 5, "4 个 text part + 1 个 reasoning part");
    let reasoning_parts = parts
        .iter()
        .filter(|p| p.data.contains("\"reasoning\""))
        .count();
    assert_eq!(reasoning_parts, 1);
}

// ---------- 2. 设置 namespace 读写与隔离 ----------

#[tokio::test]
async fn settings_namespaces_read_write_and_isolate() {
    let db = fresh_db().await;

    // 初始为空
    let settings = db.read_settings("blackagent").await.unwrap();
    assert!(settings.is_empty());

    // 写入后可读回
    db.write_local_setting("blackagent", "lastChannelKey", &serde_json::json!("ZCode|glm|M1"))
        .await
        .unwrap();
    let settings = db.read_settings("blackagent").await.unwrap();
    assert_eq!(settings.get("lastChannelKey").unwrap(), "ZCode|glm|M1");

    // 覆盖写（先查后改，不产生重复行）
    db.write_local_setting("blackagent", "lastChannelKey", &serde_json::json!("ZCode|glm|M2"))
        .await
        .unwrap();
    assert_eq!(count(&db, "local_setting").await, 1, "同 key 只有一行");
    let settings = db.read_settings("blackagent").await.unwrap();
    assert_eq!(settings.get("lastChannelKey").unwrap(), "ZCode|glm|M2");

    // namespace 隔离：两 namespace 互不可见
    db.write_local_setting("otherapp", "lastChannelKey", &serde_json::json!("Codex|t|m"))
        .await
        .unwrap();
    assert_eq!(count(&db, "local_setting").await, 2, "两 namespace 各一行");
    let blackagent = db.read_settings("blackagent").await.unwrap();
    assert_eq!(blackagent.get("lastChannelKey").unwrap(), "ZCode|glm|M2");
    let other = db.read_settings("otherapp").await.unwrap();
    assert_eq!(other.get("lastChannelKey").unwrap(), "Codex|t|m");

    // 数组值
    db.write_local_setting(
        "blackagent",
        "manualChannels",
        &serde_json::json!([{"harness":"ZCode","team":"t1","model":"m1"}]),
    )
    .await
    .unwrap();
    let settings = db.read_settings("blackagent").await.unwrap();
    assert_eq!(settings["manualChannels"][0]["harness"], "ZCode");
}

// ---------- 3. list_sessions 按 time_updated desc ----------

#[tokio::test]
async fn list_sessions_orders_by_time_updated_desc() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let s1 = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 100).await;
    let s2 = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 2, 300).await;
    let s3 = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 3, 200).await;

    let sessions = db.list_sessions("prj_1").await.unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![s2.as_str(), s3.as_str(), s1.as_str()], "按 time_updated desc");
    assert!(sessions.iter().all(|s| s.channel_key == "ZCode|t|m"));

    // touch 只前进不回退
    db.touch_session(&s1, 150).await.unwrap();
    db.touch_session(&s1, 50).await.unwrap();
    let row = db.find_session(&s1).await.unwrap().unwrap();
    assert_eq!(row.time_updated, 150, "回退触碰不生效");
    // 全量列表（load_store 用）
    let all = db.list_all_sessions().await.unwrap();
    assert_eq!(all.len(), 3);
    // project 记录往返
    let found = db.find_project("prj_1").await.unwrap().unwrap();
    assert_eq!(found.path, "/tmp/demo");
    assert!(db.project_path_exists("/tmp/demo").await.unwrap());
    assert!(!db.project_path_exists("/tmp/none").await.unwrap());
}

// ---------- 4. 存档：列表过滤 + 可恢复 ----------

#[tokio::test]
async fn archive_session_hides_and_restores() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let s1 = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 100).await;
    let s2 = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 2, 200).await;

    db.archive_session(&s1).await.unwrap();
    // 列表（按项目 / 全量）都不再含已存档会话，另一会话不受影响
    let sessions = db.list_sessions("prj_1").await.unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec![s2.as_str()]
    );
    assert!(db
        .list_all_sessions()
        .await
        .unwrap()
        .iter()
        .all(|s| s.id != s1));
    // 行数据仍在，archived_at 已置值；消息数据保留
    let row = db.find_session(&s1).await.unwrap().unwrap();
    assert!(row.time_archived.is_some(), "存档后 time_archived 置值");
    db.insert_user_message(&s1, "ZCode", "m", "hi").await.unwrap();
    assert_eq!(count(&db, "message").await, 1);
    // 取消存档后重新可见
    db.unarchive_session(&s1).await.unwrap();
    let sessions = db.list_sessions("prj_1").await.unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(db
        .find_session(&s1)
        .await
        .unwrap()
        .unwrap()
        .time_archived
        .is_none());
    // 不存在的会话要报错（Error::InvalidConfig）
    let err = db.archive_session("sess_nope").await.unwrap_err();
    assert!(err.to_string().contains("session was not found"), "实际错误: {err}");
    // delete_project 级联仍覆盖已存档会话
    db.archive_session(&s2).await.unwrap();
    let ids = db.list_session_ids("prj_1").await.unwrap();
    assert_eq!(ids.len(), 2, "已存档会话仍随项目级联删除");
    let deleted = db.delete_project_cascade("prj_1").await.unwrap();
    assert_eq!(deleted.len(), 2, "级联删除返回自查的 session id 集合");
    assert_eq!(count(&db, "session").await, 0);
}

// ---------- 5. 删除级联 ----------

#[tokio::test]
async fn delete_session_cascades() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let keep = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;
    let gone = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;

    db.insert_user_message(&gone, "ZCode", "m", "hi").await.unwrap();
    db.insert_user_message(&keep, "ZCode", "m", "stay").await.unwrap();
    assert_eq!(count(&db, "message").await, 2);
    assert_eq!(count(&db, "part").await, 2);

    db.delete_session_cascade(&gone).await.unwrap();
    assert_eq!(count(&db, "session").await, 1);
    assert_eq!(count(&db, "message").await, 1);
    assert_eq!(count(&db, "part").await, 1);
    assert_eq!(count(&db, "session_entry").await, 1, "只保留 keep 的 entry");
    let left = db.get_messages(&keep).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].content, "stay");

    // delete_project 级联清掉 keep 与项目本身
    let ids = db.list_session_ids("prj_1").await.unwrap();
    assert_eq!(ids.len(), 1);
    db.delete_project_cascade("prj_1").await.unwrap();
    assert_eq!(count(&db, "project").await, 0);
    assert_eq!(count(&db, "session").await, 0);
    assert_eq!(count(&db, "message").await, 0);
    assert_eq!(count(&db, "part").await, 0);
    assert_eq!(count(&db, "session_entry").await, 0);
}

// ---------- 6. 渠道选择 entry ----------

#[tokio::test]
async fn channel_selection_entry_upsert() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let sid = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;

    let key = db.find_channel_key(&sid).await.unwrap().unwrap();
    assert_eq!(key, "ZCode|t|m");
    // 更新（entry 固定 id，改写不新增）
    db.update_channel_selection(&sid, "Codex|t2|m2", 5).await.unwrap();
    assert_eq!(count(&db, "session_entry").await, 1);
    let key = db.find_channel_key(&sid).await.unwrap().unwrap();
    assert_eq!(key, "Codex|t2|m2");
    // 更新不存在的 entry 时新建
    db.update_channel_selection("sess_missing", "a|b|c", 6).await.unwrap();
    assert_eq!(count(&db, "session_entry").await, 2);
    assert!(db.find_channel_key("sess_none").await.unwrap().is_none());
}

// ---------- 6.5 待办状态 entry ----------

#[tokio::test]
async fn todo_state_entry_upsert_and_read_back() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let sid = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;

    // 未写过：None
    assert!(db.find_todo_state(&sid).await.unwrap().is_none());

    // 首写 → 读回一致（status snake_case / activeForm 经 TodoEntry 自身 serde）
    let snapshot = channel::TodoSnapshot {
        entries: vec![
            channel::TodoEntry {
                content: "第一步".into(),
                status: channel::TodoStatus::Completed,
                priority: Some("high".into()),
                active_form: None,
            },
            channel::TodoEntry {
                content: "第二步".into(),
                status: channel::TodoStatus::InProgress,
                priority: None,
                active_form: Some("正在第二步".into()),
            },
        ],
        explanation: Some("两步走".into()),
    };
    db.upsert_todo_state(&sid, &snapshot, 10).await.unwrap();
    assert_eq!(db.find_todo_state(&sid).await.unwrap().unwrap(), snapshot);
    assert_eq!(count(&db, "session_entry").await, 2, "渠道选择 + 待办各一行");

    // 覆盖写：仍是同一行，内容整体换成新快照
    let next = channel::TodoSnapshot {
        entries: vec![channel::TodoEntry {
            content: "唯一一步".into(),
            status: channel::TodoStatus::Completed,
            priority: None,
            active_form: None,
        }],
        explanation: None,
    };
    db.upsert_todo_state(&sid, &next, 20).await.unwrap();
    assert_eq!(count(&db, "session_entry").await, 2, "覆盖写不新增行");
    assert_eq!(db.find_todo_state(&sid).await.unwrap().unwrap(), next);

    // 会话隔离：另一会话读不到
    let other = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;
    assert!(db.find_todo_state(&other).await.unwrap().is_none());

    // 级联删除覆盖 todo entry
    db.delete_session_cascade(&sid).await.unwrap();
    assert!(db.find_todo_state(&sid).await.unwrap().is_none());
}

// ---------- 7. 会话创建与重命名语义 ----------

#[tokio::test]
async fn touch_title_and_rename() {
    let db = fresh_db().await;
    db.insert_project(&project("prj_1")).await.unwrap();
    let sid = seed_session(&db, "prj_1", "/tmp/d", "ZCode|t|m", 1, 1).await;

    // 首条消息自动标题（默认标题才改）
    let changed = db
        .auto_title_if_default(&sid, "这是一段超过会被截断的很长输入文本，用来验证二十四字符截断逻辑是否符合预期。")
        .await
        .unwrap();
    assert!(changed);
    let row = db.find_session(&sid).await.unwrap().unwrap();
    let expect: String = "这是一段超过会被截断的很长输入文本，用来验证二十四字符截断逻辑是否符合预期。"
        .chars()
        .take(24)
        .collect();
    assert_eq!(row.title, expect);
    assert_eq!(row.title_source, TITLE_SOURCE_FIRST_INPUT);
    assert!(row.time_title_updated.is_some());

    // 已非默认标题：再发消息不改标题
    let changed = db.auto_title_if_default(&sid, "第二条").await.unwrap();
    assert!(!changed);
    let row = db.find_session(&sid).await.unwrap().unwrap();
    assert_eq!(row.title, expect);

    // 重命名
    db.rename_session(&sid, "手动命名").await.unwrap();
    let row = db.find_session(&sid).await.unwrap().unwrap();
    assert_eq!(row.title, "手动命名");
    assert_eq!(row.title_source, TITLE_SOURCE_CUSTOM);

    // 重命名不存在的会话要报错（Error::InvalidConfig）
    let err = db.rename_session("nope", "x").await.unwrap_err();
    assert!(err.to_string().contains("session was not found"), "实际错误: {err}");
}

// ---------- 8. 默认路径解析（不触盘，仅形状） ----------

#[test]
fn default_path_falls_back_to_home_layout() {
    // 不强制 HOME 存在（CI 沙箱可能没有）：有 HOME 时路径形如
    // $HOME/chat_histories.db；无 HOME 时 default_path 报 Initialization。
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => {
            let path = ChatHistoryDatabase::default_path().unwrap();
            assert_eq!(path, std::path::PathBuf::from(home).join(DB_FILE_NAME));
        }
        None => {
            let err = ChatHistoryDatabase::default_path().unwrap_err();
            assert!(err.to_string().contains("home directory"));
        }
    }
}
