//! history PRAGMA 执行通道回归测试：临时文件库，不碰真实
//! `~/chat_histories.db`。
//!
//! 背景（迁移自 BlackAgent tests/db_pragma.rs，病灶注释原文保留）：
//! turso 把 journal_mode 声明为查询型 PRAGMA（无 NoColumns1，带参也
//! 返回一行当前模式），Connection::execute 遇到结果行必报 "unexpected row
//! during execution"；而切换副作用（改文件头并落盘）在产行之前已完成——
//! 修复前 init_database 对它走 execute_sql，每次启动必误报"PRAGMA 未生效
//! （忽略）"。foreign_keys / busy_timeout 带 RHS 参数时为零结果列，execute
//! 通道正常，维持原通道。:memory: 库的 journal_mode 语义与文件库不同，
//! 故一律用临时文件库。
//!
//! 运行：cargo test --features history --test history_pragma
//! 病灶复现（ignored）：cargo test --test history_pragma -- --ignored

#![cfg(feature = "history")]

use std::path::Path;

use channel::ChatHistoryDatabase;

/// 病灶复现（保留作根因证据，不进常规套件）：execute 通道对 journal_mode
/// 这类带结果行的 PRAGMA 必报 unexpected row（turso Statement::step 在
/// execute 路径收到 Row 即 Misuse）。
#[tokio::test]
#[ignore = "病灶复现：execute 通道执行带结果行的 PRAGMA 报 unexpected row"]
async fn execute_channel_rejects_journal_mode_pragma() {
    let database = pragma_test_db().await;
    let err = database
        .execute_sql(ormer::sql!("PRAGMA journal_mode=wal"))
        .await
        .expect_err("execute 对返回结果行的 PRAGMA 应报错");
    assert!(
        err.to_string().contains("unexpected row"),
        "错误应包含 unexpected row: {err}"
    );
}

/// 修复回归：open_at 里 journal_mode 走查询通道消费结果行并校验返回值；
/// 其余两条 PRAGMA 维持 execute 通道，执行后可读回生效值。
#[tokio::test]
async fn open_at_pragma_channels_apply_and_read_back() {
    let dir = tempfile::tempdir().expect("建临时目录失败");
    let path = dir.path().join("history-pragma.db");
    let db = ChatHistoryDatabase::open_at(&path)
        .await
        .expect("打开临时文件库失败");

    let conn = db.lock().await;
    let modes: Vec<String> = conn
        .select_sql::<String>(ormer::sql!("PRAGMA journal_mode"))
        .collect::<Vec<String>>()
        .await
        .expect("读回 journal_mode 失败");
    assert_eq!(modes.first().map(String::as_str), Some("wal"), "库文件应为 wal 模式");

    let foreign_keys: Vec<i64> = conn
        .select_sql::<i64>(ormer::sql!("PRAGMA foreign_keys"))
        .collect::<Vec<i64>>()
        .await
        .expect("读回 foreign_keys 失败");
    assert_eq!(foreign_keys.first(), Some(&1));
    let timeout: Vec<i64> = conn
        .select_sql::<i64>(ormer::sql!("PRAGMA busy_timeout"))
        .collect::<Vec<i64>>()
        .await
        .expect("读回 busy_timeout 失败");
    assert_eq!(timeout.first(), Some(&5000));
    drop(conn);

    // 六表迁移已跑：句柄方法在新库上可用
    db.insert_project(&channel::ProjectRecord {
        id: "prj_1".to_owned(),
        name: "d".to_owned(),
        path: "/tmp/d".to_owned(),
        created_at: 1,
    })
    .await
    .expect("句柄方法应可用（六表已迁移）");
}

/// 每测试独立的临时文件库（进程号保证唯一），测完由 tempdir 连带
/// -wal/-shm 清理。
async fn pragma_test_db() -> ormer::Database {
    let path = std::env::temp_dir().join(format!(
        "channel-history-pragma-pathogen-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    ormer::Database::connect(ormer::DbType::Sqlite, &path.display().to_string())
        .await
        .expect("连接临时测试库失败")
}

/// :memory: 之外的路径每次 open_at 都执行幂等迁移：重复打开同一文件不报错。
#[tokio::test]
async fn open_at_is_idempotent_across_reopens() {
    let dir = tempfile::tempdir().expect("建临时目录失败");
    let path = dir.path().join("history-reopen.db");
    for round in 0..3 {
        let db = ChatHistoryDatabase::open_at(&path)
            .await
            .unwrap_or_else(|error| panic!("第 {round} 次打开失败: {error}"));
        db.insert_project(&channel::ProjectRecord {
            id: format!("prj_{round}"),
            name: "d".to_owned(),
            path: format!("/tmp/{round}"),
            created_at: round,
        })
        .await
        .expect("写项目失败");
    }
    assert!(Path::new(&path).is_file());
    assert!(std::fs::metadata(&path).map(|meta| meta.len() > 0).unwrap_or(false));
}
