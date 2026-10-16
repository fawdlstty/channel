// 临时验证：锁住的 cc-switch.db 复制快照后 ormer/turso 能否读取（用后即删）。
use ormer::Database;

#[derive(ormer::Model)]
#[table = "providers"]
struct ProviderRow {
    #[primary]
    id: String,
    #[primary]
    app_type: String,
    name: String,
    settings_config: String,
    meta: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let src = format!("{}/.cc-switch/cc-switch.db", std::env::var("HOME")?);
    let dir = std::env::temp_dir().join(format!("ccsnap-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let snap = dir.join("snapshot.db");
    std::fs::copy(&src, &snap)?;
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::copy(format!("{src}{ext}"), format!("{}{ext}", snap.display()));
    }
    let db = Database::connect(ormer::DbType::Sqlite, &snap.to_string_lossy()).await?;
    let rows = db
        .select::<ProviderRow>()
        .filter(|p| p.app_type.eq("codex"))
        .collect::<Vec<_>>()
        .await?;
    println!("codex rows via snapshot: {}", rows.len());
    for row in rows.iter().take(3) {
        println!("  - {}", row.name);
    }
    // 顺手验证 opencode 的 app_type 读取
    let rows = db
        .select::<ProviderRow>()
        .filter(|p| p.app_type.eq("opencode"))
        .collect::<Vec<_>>()
        .await?;
    for row in rows {
        println!("opencode: {} -> {}", row.name, &row.settings_config[..row.settings_config.len().min(120)]);
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
