//! history 并发回归测试：全部跑 :memory:，不落盘、不碰真实
//! `~/chat_histories.db`。
//!
//! 背景（迁移自 BlackAgent tests/db_concurrency.rs，病灶注释原文保留）：
//! ormer 的 SQLite Database 内部是单条 turso 连接，同一连接上两个查询
//! 的执行窗口重叠会报 `Rows::next failed: concurrent use forbidden`（turso
//! 对连接上所有语句共享一把 ConcurrentGuard）。修复前 AppState.db 裸共享，
//! 启动时前端并行发 load_store + list_channels 即可撞出。
//!
//! 运行：cargo test --features history --test history_concurrency
//! 病灶复现（ignored）：cargo test --test history_concurrency -- --ignored

#![cfg(feature = "history")]

use std::path::Path;
use std::sync::Arc;

use channel::ChatHistoryDatabase;

/// 病灶复现（保留作根因证据，不进常规套件）：多个任务共享裸
/// ormer::Database（迁移前宿主 AppState.db 的用法），多线程 runtime 下
/// 同一连接的查询执行窗口重叠即撞 guard。竞态强度高时大概率复现，但本质
/// 仍是概率性的——复现失败可加大轮次重跑。与迁移来源的差异：channel 的
/// migrate_schema 是私有函数（宿主不需要在句柄外建表），裸连接上直接跑
/// `SELECT 1` 原生查询——turso 的 ConcurrentGuard 在连接层，与查询形态
/// 无关，病灶机理一致。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "病灶复现：并发共享裸连接报 concurrent use forbidden；修复后已无此用法"]
async fn raw_shared_connection_reproduces_concurrent_use_forbidden() {
    let database = Arc::new(
        ormer::Database::connect(ormer::DbType::Sqlite, ":memory:")
            .await
            .expect("连接 :memory: 失败"),
    );
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let db = database.clone();
        set.spawn(async move {
            let mut forbidden = 0;
            for _ in 0..300 {
                let ones = db
                    .select_sql::<i64>(ormer::sql!("SELECT 1"))
                    .collect::<Vec<i64>>()
                    .await;
                if let Err(err) = ones {
                    if err.to_string().contains("concurrent use forbidden") {
                        forbidden += 1;
                    }
                }
            }
            forbidden
        });
    }
    let mut total = 0;
    while let Some(forbidden) = set.join_next().await {
        total += forbidden.expect("任务 panic");
    }
    eprintln!("病灶复现：{total} 次并发冲突");
    assert!(total > 0, "未复现并发冲突（竞态未触发，可加大轮次重跑）");
}

/// 修复回归：同样强度的并发炮轰，所有操作经 ChatHistoryDatabase 互斥串行，
/// 零错误。混合读设置/项目/会话/消息（含切片与 boundary 查询）与交替写
/// 设置，覆盖启动并行加载与运行期命令并发的真实形态。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handle_serializes_concurrent_command_storm() {
    let db = Arc::new(
        ChatHistoryDatabase::open_at(Path::new(":memory:"))
            .await
            .expect("连接失败"),
    );
    db.insert_project(&channel::ProjectRecord {
        id: "prj_test".to_owned(),
        name: "测试项目".to_owned(),
        path: "/tmp".to_owned(),
        created_at: 0,
    })
    .await
    .expect("写项目失败");
    db.write_local_setting(
        "blackagent",
        "lastChannelKey",
        &serde_json::json!("harness|team|model"),
    )
    .await
    .expect("写设置失败");

    let mut set = tokio::task::JoinSet::new();
    for worker in 0..8 {
        let db = db.clone();
        set.spawn(async move {
            for round in 0..300u32 {
                if worker % 2 == 0 {
                    // 读侧：设置 + 项目 + 会话 + 消息/切片（load_store /
                    // list_channels 形态，含压缩读原语）
                    db.read_settings("blackagent").await.expect("读设置失败");
                    db.list_projects().await.expect("读项目失败");
                    db.list_all_sessions().await.expect("读会话失败");
                    db.get_messages("sess_none").await.expect("读消息失败");
                    db.get_context_messages("sess_none")
                        .await
                        .expect("读切片失败");
                    db.latest_compaction_boundary("sess_none")
                        .await
                        .expect("读 boundary 失败");
                } else {
                    // 写侧：交替写两个设置 key（先查后写的读改写形态）
                    let key = if round % 2 == 0 { "lastChannelKey" } else { "manualChannels" };
                    let value = if round % 2 == 0 {
                        serde_json::json!(format!("harness|team|{round}"))
                    } else {
                        serde_json::json!([{"harness": "h", "team": "t", "model": "m"}])
                    };
                    db.write_local_setting("blackagent", key, &value)
                        .await
                        .expect("写设置失败");
                    db.project_path_exists("/tmp").await.expect("查项目失败");
                }
            }
        });
    }
    while let Some(done) = set.join_next().await {
        done.expect("任务 panic");
    }
}
