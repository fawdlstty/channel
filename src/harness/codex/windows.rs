use std::path::{Path, PathBuf};

pub(super) fn find_codex() -> Option<PathBuf> {
    find_chatgpt_codex().or_else(find_vscode_codex)
}

fn find_chatgpt_codex() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    newest_executable(codex_candidates(&root))
}

fn find_vscode_codex() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("USERPROFILE")?)
        .join(".vscode")
        .join("extensions");
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        let is_chatgpt = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("openai.chatgpt-"));
        if is_chatgpt {
            candidates.extend(codex_candidates(&path));
        }
    }
    newest_executable(candidates)
}

fn codex_candidates(root: &Path) -> Vec<(std::time::SystemTime, PathBuf)> {
    let mut candidates = Vec::new();
    collect_codex_candidates(root, &mut candidates);
    candidates
}

fn collect_codex_candidates(root: &Path, candidates: &mut Vec<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_codex_candidates(&path, candidates);
        } else if path.file_name().is_some_and(|name| name == "codex.exe") {
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((modified, path));
        }
    }
}

fn newest_executable(candidates: Vec<(std::time::SystemTime, PathBuf)>) -> Option<PathBuf> {
    let mut candidates = candidates;
    candidates.sort_by(|(left, _), (right, _)| right.cmp(left));
    candidates.into_iter().map(|(_, path)| path).find(|path| {
        std::process::Command::new(path)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}
