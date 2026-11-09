//! Persistent memory for the engine (ZCode memory semantics): a
//! per-project memory directory (`~/.blackagent/cli/memories/projects/
//! <projectKey>/memory/`) with a `MEMORY.md` line index plus one `.md`
//! file per fact; the index is injected into the system prompt each turn
//! and writes happen through the model's own Write/Edit tools guided by
//! the memory instructions appended to the system prompt.

use std::path::PathBuf;

/// Root of the engine config file (`~/.blackagent/cli/config.json`) —
/// mcp.servers / hooks / permission live here.
pub fn engine_config_path() -> PathBuf {
    home_dir()
        .join(".blackagent")
        .join("cli")
        .join("config.json")
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Whether persistent memory is on (`~/.blackagent/cli/config.json →
/// memory.enabled`; default off, matching the ZCode memory switch).
pub fn memory_enabled() -> bool {
    let Ok(text) = std::fs::read_to_string(engine_config_path()) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    json.pointer("/memory/enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Derives the ZCode-shaped project key from a workspace path:
/// `<slug>-<hash16>` (slug = last directory segment, hash = first 16 hex
/// chars of a stable FNV-1a of the full path).
pub fn project_key(workspace: &std::path::Path) -> String {
    let text = workspace.to_string_lossy().replace('\\', "/");
    let slug: String = text
        .trim_end_matches('/')
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("default")
        .to_owned();
    // FNV-1a 64-bit.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{slug}-{hash:016x}")
}

/// The memory directory for one project.
pub fn memory_dir(workspace: &std::path::Path) -> PathBuf {
    home_dir()
        .join(".blackagent")
        .join("cli")
        .join("memories")
        .join("projects")
        .join(project_key(workspace))
        .join("memory")
}

/// The index file (one line per memory: `- [title](file.md) — hook`).
pub fn memory_index_path(workspace: &std::path::Path) -> PathBuf {
    memory_dir(workspace).join("MEMORY.md")
}

/// Reads the current index lines (empty when memory is off/absent).
pub fn read_index(workspace: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(memory_index_path(workspace)) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- ["))
        .map(str::to_owned)
        .collect()
}

/// The system-prompt section injected each turn when memory is enabled
/// (index + the write contract, mirroring the ZCode memory instructions).
pub fn system_prompt_section(workspace: &std::path::Path) -> String {
    let dir = memory_dir(workspace);
    let index = read_index(workspace);
    let index_text = if index.is_empty() {
        String::new()
    } else {
        format!("当前记忆索引：\n{}\n\n", index.join("\n"))
    };
    format!(
        "\n## 持久记忆\n\n你拥有跨会话持久记忆（目录 {}）。规则：\n\
         1. 索引文件 MEMORY.md 每条记忆一行：`- [标题](文件名.md) — 一句话钩子`。\n\
         2. 每条记忆一个 .md 文件，frontmatter 含 name（与文件名相同的 slug）/description/metadata.type（user|feedback|project|reference 四选一）。\n\
         3. 值得记：主人的身份与偏好（user）、主人给的工作方式反馈（feedback）、不可从代码/git 看出的项目目标与约束（project）、外部资源指针（reference）。代码结构、本次对话细节不记。\n\
         4. 写新记忆用 Write 新建文件并同步在 MEMORY.md 追加索引行；更新已有记忆用 Edit。\n\
         5. 只有在获得明确可复用信息时才写入，宁缺勿滥。\n\n{index_text}",
        dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_is_slug_plus_stable_hash() {
        let key = project_key(std::path::Path::new(r"E:\GitHub_fa\BlackAgent"));
        assert!(key.starts_with("BlackAgent-"), "{key}");
        assert_eq!(key.len(), "BlackAgent-".len() + 16);
        // Stable across calls, different across paths.
        assert_eq!(key, project_key(std::path::Path::new("E:/GitHub_fa/BlackAgent")));
        assert_ne!(key, project_key(std::path::Path::new(r"E:\GitHub_fa\channel")));
    }
}
