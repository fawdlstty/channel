//! Engine tool surface — ZCode-style built-in tools for the in-process
//! agent loop.
//!
//! Tool list (first wave, ZCode schemas):
//! - `Bash`    {command, timeout?, description?, run_in_background?}
//! - `Read`    {file_path, offset?, limit?} — cat -n numbered output
//! - `Write`   {file_path, content} — overwrite requires a prior Read
//! - `Edit`    {file_path, old_string, new_string, replace_all?} — requires a
//!   prior Read; old_string must match exactly and uniquely
//! - `Glob`    {pattern, path?} — mtime-sorted, 100-entry truncation
//! - `Grep`    {pattern, path?, glob?, output_mode?, head_limit?}
//! - `TodoWrite` / `TodoRead` — full-replacement todo snapshots
//! - `TaskOutput` / `TaskStop` — background task plumbing
//! - `AskUserQuestion` / `EnterPlanMode` / `ExitPlanMode` — interaction
//!   tools bridged through the backend's request/response wiring
//!
//! Handlers receive [`ToolContext`] (shared per-session state) and the
//! parsed JSON arguments, and return the tool result text (or an error
//! message that is reported to the model as a failed tool call).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use regex::Regex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::TodoEntry;
use crate::utils::text::{decode_child_output, decode_file_bytes};

use super::automation::AutomationStore;

// ---------------------------------------------------------------------------
// Shared per-session state
// ---------------------------------------------------------------------------

/// A background Bash task: killable child plus accumulated output.
pub struct BackgroundTask {
    pub id: u64,
    pub display: String,
    pub kill: tokio::sync::mpsc::Sender<()>,
    /// Finished flag flipped by the runner task; output tail kept for
    /// TaskOutput reads.
    pub state: Arc<Mutex<BackgroundState>>,
}

#[derive(Default)]
pub struct BackgroundState {
    pub finished: bool,
    pub notified: bool,
    pub exit_status: Option<String>,
    pub output: String,
}

/// What kind of interaction a pending request carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractionKind {
    /// AskUserQuestion: the answer is `{"answers": {...}}` shaped like the
    /// respond_user_input map (question id → labels).
    Ask,
    /// ExitPlanMode: the answer is `{"approved": true|false}`.
    Plan,
}

/// A request from a tool handler to the user (AskUserQuestion / plan
/// approval). The loop driver owns the response side: it emits the
/// corresponding wire event and routes the host's answer back through
/// `responder`.
pub struct InteractRequest {
    pub id: String,
    pub kind: InteractionKind,
    /// AskUserQuestion: `{"questions": [...]}`; ExitPlanMode:
    /// `{"plan": "..."}`; EnterPlanMode: `{}`.
    pub payload: Value,
    pub responder: oneshot::Sender<Value>,
}

/// Shared per-session state handed to every tool handler.
pub struct ToolContext {
    /// Persistent working directory (ZCode Bash semantics).
    pub cwd: Mutex<PathBuf>,
    /// Files read this session (Edit/Write prior-read requirement).
    pub read_files: Mutex<std::collections::HashSet<PathBuf>>,
    /// Current todo snapshot (TodoWrite full replacement).
    pub todos: Mutex<Vec<TodoEntry>>,
    /// Background Bash tasks by id.
    pub background: Mutex<HashMap<u64, Arc<BackgroundTask>>>,
    pub background_counter: AtomicU64,
    /// Interaction bridge to the loop driver (Ask/plan tools).
    pub interact: mpsc::Sender<InteractRequest>,
    pub interact_counter: AtomicU64,
    /// Automation store bridge (Cron tools); `None` when the host did not
    /// wire a scheduler.
    pub automation: Mutex<Option<Arc<dyn AutomationStore>>>,
    /// Subagent host bridge (Agent/SendMessage tools); `None` disables the
    /// subagent tool surface.
    pub agent_host: Mutex<Option<Arc<super::agents::AgentHost>>>,
    /// Session id for hook payloads.
    pub session_id: String,
}

impl ToolContext {
    pub fn new(workspace: &Path, session_id: String, interact: mpsc::Sender<InteractRequest>) -> Self {
        Self {
            cwd: Mutex::new(workspace.to_path_buf()),
            read_files: Mutex::new(Default::default()),
            todos: Mutex::new(Vec::new()),
            background: Mutex::new(HashMap::new()),
            background_counter: AtomicU64::new(1),
            interact,
            interact_counter: AtomicU64::new(1),
            automation: Mutex::new(None),
            agent_host: Mutex::new(None),
            session_id,
        }
    }

    /// The wired automation store, if any.
    pub fn automation_store(&self) -> Option<Arc<dyn AutomationStore>> {
        self.automation.lock().unwrap().clone()
    }

    /// The wired subagent host, if any.
    pub fn agent_host(&self) -> Option<Arc<super::agents::AgentHost>> {
        self.agent_host.lock().unwrap().clone()
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            self.cwd.lock().unwrap().join(candidate)
        }
    }

    fn mark_read(&self, path: &Path) {
        self.read_files.lock().unwrap().insert(path.to_path_buf());
    }

    fn was_read(&self, path: &Path) -> bool {
        self.read_files.lock().unwrap().contains(path)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Spawns without a console window on Windows (CREATE_NO_WINDOW); no-op on
/// other platforms.
pub(crate) fn hide_console(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    #[cfg(not(windows))]
    let _ = cmd;
}

pub type Handler = for<'a> fn(&'a ToolContext, &'a Value) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
>;

pub struct ToolEntry {
    pub spec: crate::llm::ToolSpec,
    pub handler: Handler,
}

/// The full first-wave registry (plus MCP tools appended by the loop).
pub fn registry() -> Vec<ToolEntry> {
    let entries: Vec<ToolEntry> = vec![
        tool("Bash", &bash_description(), bash_schema(), run_bash),
        tool("Read", READ_DESCRIPTION, read_schema(), run_read),
        tool("Write", WRITE_DESCRIPTION, write_schema(), run_write),
        tool("Edit", EDIT_DESCRIPTION, edit_schema(), run_edit),
        tool("Glob", GLOB_DESCRIPTION, glob_schema(), run_glob),
        tool("Grep", GREP_DESCRIPTION, grep_schema(), run_grep),
        tool("TodoWrite", TODO_WRITE_DESCRIPTION, todo_write_schema(), run_todo_write),
        tool("TodoRead", TODO_READ_DESCRIPTION, empty_schema(), run_todo_read),
        tool("TaskOutput", TASK_OUTPUT_DESCRIPTION, task_output_schema(), run_task_output),
        tool("TaskStop", TASK_STOP_DESCRIPTION, task_stop_schema(), run_task_stop),
        tool(
            "AskUserQuestion",
            ASK_USER_QUESTION_DESCRIPTION,
            ask_user_question_schema(),
            run_ask_user_question,
        ),
        tool("EnterPlanMode", ENTER_PLAN_MODE_DESCRIPTION, empty_schema(), run_enter_plan_mode),
        tool("ExitPlanMode", EXIT_PLAN_MODE_DESCRIPTION, exit_plan_mode_schema(), run_exit_plan_mode),
        tool("CronCreate", CRON_CREATE_DESCRIPTION, cron_create_schema(), run_cron_create),
        tool("CronList", CRON_LIST_DESCRIPTION, empty_schema(), run_cron_list),
        tool("CronUpdate", CRON_UPDATE_DESCRIPTION, cron_update_schema(), run_cron_update),
        tool("CronDelete", CRON_DELETE_DESCRIPTION, cron_delete_schema(), run_cron_delete),
    ];
    entries
}

fn tool(name: &str, description: &str, parameters: Value, handler: Handler) -> ToolEntry {
    ToolEntry {
        spec: crate::llm::ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
        },
        handler,
    }
}

/// Dispatches one tool call by name against the registry. Unknown tools
/// return an error text (the model sees it as the tool result).
pub async fn dispatch(ctx: &ToolContext, name: &str, args: &Value) -> Result<String, String> {
    // Subagent tools are registered dynamically by the agents module.
    if let Some(result) = super::agents::try_dispatch(ctx, name, args).await {
        return result;
    }
    dispatch_registry(ctx, name, args).await
}

/// Dispatch against the static registry only (no subagent tools). The
/// subagent loop uses this: Agent/SendMessage must not recurse through the
/// dispatch graph (depth-1 enforced semantically, recursion is broken
/// structurally here).
pub async fn dispatch_registry(
    ctx: &ToolContext,
    name: &str,
    args: &Value,
) -> Result<String, String> {
    let entry = registry()
        .into_iter()
        .find(|entry| entry.spec.name == name)
        .ok_or_else(|| format!("未知工具: {name}"))?;
    (entry.handler)(ctx, args).await
}

fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
}

// ---------------------------------------------------------------------------
// Bash
// ---------------------------------------------------------------------------

fn bash_description() -> String {
    super::shell::bash_description()
}

fn bash_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "The command to execute"},
            "timeout": {"type": "number", "description": "Optional timeout in milliseconds (max 600000)"},
            "description": {"type": "string", "description": "Clear, concise description of what this command does in active voice"},
            "run_in_background": {"type": "boolean", "description": "Set to true to run this command in the background"}
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

const BASH_DEFAULT_TIMEOUT_MS: u64 = 120_000;
const BASH_MAX_TIMEOUT_MS: u64 = 600_000;
const BASH_OUTPUT_LIMIT: usize = 30_000;

fn truncate_output(output: &str) -> String {
    if output.len() <= BASH_OUTPUT_LIMIT {
        return output.to_owned();
    }
    let mut cut = BASH_OUTPUT_LIMIT;
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[输出已截断，共 {} 字节]", &output[..cut], output.len())
}

/// Drains one pipe of a background child into the shared output buffer.
/// Bytes accumulate raw and are decoded once at EOF：分块逐段 lossy 会把
/// 跨块切开的 UTF-8 多字节字符读坏，也会把 GBK 输出读成替换符串。
async fn drain_pipe(pipe: Option<impl tokio::io::AsyncRead + Unpin>, out: Arc<Mutex<Vec<u8>>>) {
    use tokio::io::AsyncReadExt;
    let mut pipe = match pipe {
        Some(pipe) => pipe,
        None => return,
    };
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => out.lock().unwrap().extend_from_slice(&buf[..n]),
        }
    }
}

/// Persistent-cwd plumbing for the Bash tool (ZCode cwdFilePath semantics).
///
/// POSIX dialects (Git Bash on Windows, $SHELL on Unix) run the command
/// wrapped in a brace group that preserves the exit code and drops `pwd`
/// into a per-session temp file; the foreground path reads it back and
/// updates [`ToolContext::cwd`]. The cmd fallback (Windows legacy shell)
/// cannot wrap safely (`A & B` erases A's exit code), so it keeps the raw
/// command and its description says cwd does not persist.
fn cwd_persist_file(ctx: &ToolContext) -> Option<PathBuf> {
    match super::shell::resolve().dialect {
        super::shell::ShellDialect::GitBash | super::shell::ShellDialect::Posix => {
            Some(std::env::temp_dir().join(format!("blackagent-cwd-{}.txt", ctx.session_id)))
        }
        super::shell::ShellDialect::Cmd => None,
    }
}

/// Wraps a command with the cwd probe (empty commands are passed through
/// bare: `{ \n}` alone is a bash syntax error).
fn wrap_with_cwd_probe(command: &str, cwd_file: &Path) -> String {
    let target = cwd_file.to_string_lossy().replace('\\', "/");
    format!("{{ {command}\n}} ; __ba_rc=$? ; pwd > \"{target}\" ; exit $__ba_rc")
}

/// Applies a reported cwd (POSIX `/e/foo` form on Windows) back onto the
/// session's persistent working directory.
fn apply_reported_cwd(ctx: &ToolContext, cwd_file: &Path) {
    let Ok(text) = std::fs::read_to_string(cwd_file) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // Compile-time gates: `posix_to_windows_path` only exists on Windows.
    #[cfg(windows)]
    let next =
        super::shell::posix_to_windows_path(trimmed).unwrap_or_else(|| PathBuf::from(trimmed));
    #[cfg(not(windows))]
    let next = PathBuf::from(trimmed);
    if next.is_dir() {
        *ctx.cwd.lock().unwrap() = next;
    }
}

fn run_bash<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let command = args["command"]
            .as_str()
            .ok_or("缺少 command 参数")?
            .to_owned();
        let timeout_ms = args["timeout"]
            .as_u64()
            .unwrap_or(BASH_DEFAULT_TIMEOUT_MS)
            .min(BASH_MAX_TIMEOUT_MS);
        let background = args["run_in_background"].as_bool().unwrap_or(false);
        let cwd = ctx.cwd.lock().unwrap().clone();
        // Foreground POSIX runs carry the cwd probe (wrapper reports `pwd`
        // after the command); background and cmd-fallback runs stay raw.
        let cwd_probe = if background { None } else { cwd_persist_file(ctx) };
        let (program, argv) = match (&cwd_probe, command.trim().is_empty()) {
            (Some(file), false) => super::shell::invocation(&wrap_with_cwd_probe(&command, file)),
            _ => super::shell::invocation(&command),
        };

        if background {
            let task_id = ctx.background_counter.fetch_add(1, Ordering::Relaxed);
            let state = Arc::new(Mutex::new(BackgroundState::default()));
            let (kill_tx, mut kill_rx) = tokio::sync::mpsc::channel::<()>(1);
            let mut cmd = tokio::process::Command::new(&program);
            cmd.args(&argv).current_dir(&cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            hide_console(&mut cmd);
            let mut child = cmd.spawn().map_err(|err| format!("启动后台命令失败: {err}"))?;
            let runner_state = state.clone();
            let display = command.lines().next().unwrap_or(&command).to_owned();
            tokio::spawn(async move {
                let stdout_pipe = child.stdout.take();
                let stderr_pipe = child.stderr.take();
                let acc_out = Arc::new(Mutex::new(Vec::new()));
                let acc_err = Arc::new(Mutex::new(Vec::new()));
                let reader_out =
                    tokio::spawn(drain_pipe(stdout_pipe, acc_out.clone()));
                let reader_err = tokio::spawn(drain_pipe(stderr_pipe, acc_err.clone()));
                let status = tokio::select! {
                    _ = kill_rx.recv() => {
                        let _ = child.start_kill();
                        "killed".to_owned()
                    }
                    status = child.wait() => match status {
                        Ok(status) => status.to_string(),
                        Err(err) => format!("wait failed: {err}"),
                    }
                };
                // Pipes reach EOF once the (possibly killed) child exits;
                // give the readers a moment to finish flushing.
                let _ = reader_out.await;
                let _ = reader_err.await;
                let output = {
                    let mut bytes = acc_out.lock().unwrap().clone();
                    bytes.extend_from_slice(&acc_err.lock().unwrap());
                    decode_child_output(&bytes)
                };
                let mut guard = runner_state.lock().unwrap();
                guard.finished = true;
                guard.exit_status = Some(status);
                guard.output = output;
            });
            ctx.background.lock().unwrap().insert(
                task_id,
                Arc::new(BackgroundTask {
                    id: task_id,
                    display,
                    kill: kill_tx,
                    state,
                }),
            );
            return Ok(format!(
                "后台任务已启动（task_id: {task_id}）。命令在后台运行，完成后会收到通知；可用 TaskOutput 查看输出。"
            ));
        }

        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&argv).current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().map_err(|err| format!("启动命令失败: {err}"))?;
        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| format!("命令超时（{}s）", timeout_ms / 1000))?
        .map_err(|err| format!("命令执行失败: {err}"))?;
        // Persistent cwd (POSIX dialects): the wrapper reported `pwd` after
        // the command ran; fold it back onto the session working directory.
        if let Some(cwd_file) = &cwd_probe {
            apply_reported_cwd(ctx, cwd_file);
        }
        // cmd/PowerShell 旧程序的管道输出按系统码页（zh-CN 为 GBK）写出，
        // 统一走 BOM/UTF-8/码页探测解码，避免替换符乱码。
        let mut text = decode_child_output(&output.stdout);
        let stderr = decode_child_output(&output.stderr);
        if !stderr.trim().is_empty() {
            text.push_str(&stderr);
        }
        if !output.status.success() && text.trim().is_empty() {
            return Err(format!("命令退出码 {:?}，无输出", output.status.code()));
        }
        Ok(truncate_output(&text))
    })
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

const READ_DESCRIPTION: &str = "Reads a file from the local filesystem. Output uses cat -n format (line number + tab prefix). Pass offset/limit to page through large files; default reads up to 2000 lines.";

fn read_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "The absolute path to the file to read"},
            "offset": {"type": "number", "description": "The line number to start reading from (0-based)"},
            "limit": {"type": "number", "description": "The number of lines to read (default 2000)"}
        },
        "required": ["file_path"],
        "additionalProperties": false
    })
}

fn run_read<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let path = ctx.resolve(
            args["file_path"].as_str().ok_or("缺少 file_path 参数")?,
        );
        let metadata = std::fs::metadata(&path).map_err(|err| {
            if path.is_dir() {
                format!("该路径是目录，无法作为文件读取: {}", path.display())
            } else {
                format!("读取文件失败: {err}")
            }
        })?;
        if metadata.is_dir() {
            return Err(format!("该路径是目录: {}", path.display()));
        }
        let bytes = std::fs::read(&path).map_err(|err| format!("读取文件失败: {err}"))?;
        let text = decode_file_bytes(&bytes);
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(2000).max(1) as usize;
        let mut out = String::new();
        for (index, line) in text.lines().skip(offset).take(limit).enumerate() {
            out.push_str(&format!("{}\t{}\n", offset + index + 1, line));
        }
        ctx.mark_read(&path);
        if out.is_empty() {
            return Ok("[空文件或读取范围为空]".to_owned());
        }
        Ok(out)
    })
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

const WRITE_DESCRIPTION: &str = "Writes a file to the local filesystem, overwriting if it exists. Overwriting an existing file requires reading it first (use Read); use Edit for partial changes.";

fn write_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "The absolute path to the file to write"},
            "content": {"type": "string", "description": "The content to write"}
        },
        "required": ["file_path", "content"],
        "additionalProperties": false
    })
}

fn run_write<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let path = ctx.resolve(
            args["file_path"].as_str().ok_or("缺少 file_path 参数")?,
        );
        let content = args["content"].as_str().unwrap_or_default();
        if path.exists() && !ctx.was_read(&path) {
            return Err(format!(
                "文件已存在但本会话尚未读取过（{}）。请先用 Read 读取再覆写",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("创建目录失败: {err}"))?;
        }
        // PowerShell 5.x 读无 BOM 脚本按系统 ANSI 码页（zh-CN 为 GBK）解码，
        // UTF-8 脚本内容会被误读乱导致执行失败——ps1 统一写 UTF-8 BOM。
        let bytes: Vec<u8> = if path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("ps1"))
            .unwrap_or(false)
        {
            let mut bytes = Vec::with_capacity(content.len() + 3);
            bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            bytes.extend_from_slice(content.as_bytes());
            bytes
        } else {
            content.as_bytes().to_vec()
        };
        std::fs::write(&path, &bytes).map_err(|err| format!("写入文件失败: {err}"))?;
        ctx.mark_read(&path);
        Ok(format!("已写入 {}（{} 字节）", path.display(), bytes.len()))
    })
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

const EDIT_DESCRIPTION: &str = "Performs exact string replacement in a file. The file must have been read this session; old_string must match exactly (including indentation) and be unique unless replace_all is true.";

fn edit_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "The absolute path to the file to modify"},
            "old_string": {"type": "string", "description": "The text to replace"},
            "new_string": {"type": "string", "description": "The text to replace it with (must be different)"},
            "replace_all": {"type": "boolean", "description": "Replace all occurrences (default false)"}
        },
        "required": ["file_path", "old_string", "new_string"],
        "additionalProperties": false
    })
}

fn run_edit<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let path = ctx.resolve(
            args["file_path"].as_str().ok_or("缺少 file_path 参数")?,
        );
        let old_string = args["old_string"]
            .as_str()
            .ok_or("缺少 old_string 参数")?
            .to_owned();
        let new_string = args["new_string"]
            .as_str()
            .ok_or("缺少 new_string 参数")?
            .to_owned();
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        if old_string == new_string {
            return Err("old_string 与 new_string 相同".to_owned());
        }
        if !ctx.was_read(&path) {
            return Err(format!(
                "本会话尚未读取过该文件（{}）。编辑前必须先 Read",
                path.display()
            ));
        }
        let bytes = std::fs::read(&path).map_err(|err| format!("读取文件失败: {err}"))?;
        let text = String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 文本".to_owned())?;
        let count = text.matches(&old_string).count();
        if count == 0 {
            return Err("old_string 在文件中未找到（需精确匹配，含缩进）".to_owned());
        }
        if count > 1 && !replace_all {
            return Err(format!(
            "old_string 在文件中出现 {count} 次，不唯一。提供更多上下文使其唯一，或使用 replace_all。"
        ));
        }
        let updated = if replace_all {
            text.replace(&old_string, &new_string)
        } else {
            text.replacen(&old_string, &new_string, 1)
        };
        std::fs::write(&path, &updated).map_err(|err| format!("写入文件失败: {err}"))?;
        let replacements = if replace_all { count } else { 1 };
        Ok(format!(
            "已编辑 {}（{replacements} 处替换）",
            path.display()
        ))
    })
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

const GLOB_DESCRIPTION: &str = "Fast file-pattern matching tool. Patterns use glob syntax (e.g. \"**/*.rs\"). Results are sorted by modification time; returns at most 100 paths with a truncation note.";

fn glob_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "The glob pattern to match files against"},
            "path": {"type": "string", "description": "The directory to search in (default: working directory)"}
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

const GLOB_LIMIT: usize = 100;

/// Glob matcher: `*` matches within one path component, `**` crosses
/// directories, `?` matches one non-separator char. Backslashes in the
/// path are normalized to `/` before matching (Windows input).
/// Examples: `*.rs` matches only `main.rs`; `**/*.rs` also matches
/// `src/a/main.rs`.
fn glob_match(pattern: &str, path: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/");
    let pattern = norm(pattern);
    let path = norm(path);
    let pats: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    fn match_components(pats: &[&str], parts: &[&str]) -> bool {
        if pats.is_empty() {
            return parts.is_empty();
        }
        let (head, rest) = (pats[0], &pats[1..]);
        if head == "**" {
            // `**` matches zero or more components.
            for skip in 0..=parts.len() {
                if match_components(rest, &parts[skip..]) {
                    return true;
                }
            }
            return false;
        }
        if parts.is_empty() {
            return false;
        }
        if glob_component(head, parts[0]) {
            match_components(rest, &parts[1..])
        } else {
            false
        }
    }
    match_components(&pats, &parts)
}

/// Single-component glob: `*` wildcard run, `?` single char.
fn glob_component(pattern: &str, name: &str) -> bool {
    fn inner(p: &[u8], n: &[u8]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some(b'*'), _) => {
                for skip in 0..=n.len() {
                    if inner(&p[1..], &n[skip..]) {
                        return true;
                    }
                }
                false
            }
            (Some(b'?'), Some(_)) => inner(&p[1..], &n[1..]),
            (Some(&c), Some(&d)) if c == d => inner(&p[1..], &n[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), name.as_bytes())
}

/// Depth-first collector shared by Glob/Grep (depth guard against cycles;
/// Glob sorts by mtime afterwards, Grep consumes candidates directly).
fn collect_files(base: &Path, pattern: &str, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, pattern, out, depth + 1);
        } else if glob_match(pattern, &path.to_string_lossy()) {
            out.push(path);
        }
    }
}


/// Glob tool: collect matches, sort newest-first, cap at 100 entries.
fn run_glob<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let pattern = args["pattern"]
            .as_str()
            .ok_or("缺少 pattern 参数")?
            .to_owned();
        let base = match args["path"].as_str() {
            Some(path) => ctx.resolve(path),
            None => ctx.cwd.lock().unwrap().clone(),
        };
        if !base.is_dir() {
            return Err(format!("搜索目录不存在: {}", base.display()));
        }
        let mut files = Vec::new();
        collect_files(&base, &pattern, &mut files, 0);
        files.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
        });
        files.reverse(); // newest first
        let truncated = files.len() > GLOB_LIMIT;
        files.truncate(GLOB_LIMIT);
        let lines: Vec<String> = files
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        if lines.is_empty() {
            return Ok(format!("未找到匹配 {pattern} 的文件"));
        }
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!("\n[结果已截断，仅显示前 {GLOB_LIMIT} 项]"));
        }
        Ok(out)
    })
}

// ---------------------------------------------------------------------------
// Grep
// ---------------------------------------------------------------------------

const GREP_DESCRIPTION: &str = "Searches file contents with a regular expression (Rust regex syntax). output_mode: \"files_with_matches\" (default) | \"content\" | \"count\". Respects a glob filter for file names.";

fn grep_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "The regular expression pattern"},
            "path": {"type": "string", "description": "File or directory to search (default: working directory)"},
            "glob": {"type": "string", "description": "Optional glob filter for file names (e.g. \"*.ts\")"},
            "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
            "head_limit": {"type": "number", "description": "Maximum number of results (default 100)"}
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

fn run_grep<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let pattern = args["pattern"]
            .as_str()
            .ok_or("缺少 pattern 参数")?
            .to_owned();
        let regex = Regex::new(&pattern)
            .map_err(|err| format!("正则表达式无效: {err}"))?;
        let target = match args["path"].as_str() {
            Some(path) => ctx.resolve(path),
            None => ctx.cwd.lock().unwrap().clone(),
        };
        let glob = args["glob"].as_str().map(str::to_owned);
        let output_mode = args["output_mode"].as_str().unwrap_or("files_with_matches");
        let head_limit = args["head_limit"].as_u64().unwrap_or(100) as usize;

        let mut candidates = Vec::new();
        if target.is_file() {
            candidates.push(target);
        } else if target.is_dir() {
            collect_files(&target, glob.as_deref().unwrap_or("**/*"), &mut candidates, 0);
        } else {
            return Err(format!("搜索路径不存在: {}", target.display()));
        }

        let mut matched_files = 0usize;
        let mut matched_paths: Vec<String> = Vec::new();
        let mut lines_out: Vec<String> = Vec::new();
        let mut counts_out: Vec<String> = Vec::new();
        for path in &candidates {
            if lines_out.len() >= head_limit && counts_out.len() >= head_limit {
                break;
            }
            let Ok(bytes) = std::fs::read(path) else { continue };
            // Heuristic binary skip: NUL in the first 8 KiB.
            if bytes[..bytes.len().min(8192)].contains(&0) {
                continue;
            }
            let text = decode_file_bytes(&bytes);
            let mut file_count = 0usize;
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    file_count += 1;
                    if output_mode == "content" && lines_out.len() < head_limit {
                        lines_out.push(format!(
                            "{}:{}: {}",
                            path.display(),
                            index + 1,
                            line.trim_end()
                        ));
                    }
                }
            }
            if file_count > 0 {
                matched_files += 1;
                matched_paths.push(path.display().to_string());
                if output_mode == "count" && counts_out.len() < head_limit {
                    counts_out.push(format!("{}: {}", path.display(), file_count));
                }
            }
        }
        match output_mode {
            "content" => {
                if lines_out.is_empty() {
                    Ok("未找到匹配行".to_owned())
                } else {
                    Ok(lines_out.join("\n"))
                }
            }
            "count" => {
                if counts_out.is_empty() {
                    Ok("未找到匹配文件".to_owned())
                } else {
                    Ok(counts_out.join("\n"))
                }
            }
            _ => {
                if matched_paths.is_empty() {
                    Ok("未找到匹配文件".to_owned())
                } else {
                    Ok(format!(
                        "匹配文件数 {matched_files}\n{}",
                        matched_paths.join("\n")
                    ))
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// TodoWrite / TodoRead
// ---------------------------------------------------------------------------

const TODO_WRITE_DESCRIPTION: &str = "Creates and updates the task list (full replacement). Use one in_progress task at a time; mark completed immediately when done.";

fn todo_write_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "minLength": 1},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "pending | in_progress | completed"},
                        "priority": {"type": "string", "enum": ["high", "medium", "low"], "description": "high | medium | low"},
                        "active_form": {"type": "string", "description": "Present-tense form shown while in_progress (claude style)"}
                    },
                    "required": ["content", "status", "priority"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["todos"],
        "additionalProperties": false
    })
}

fn run_todo_write<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let todos = args["todos"].as_array().ok_or("缺少 todos 参数")?;
        let mut entries = Vec::new();
        for todo in todos {
            let content = todo["content"]
                .as_str()
                .ok_or("条目缺少 content")?
                .to_owned();
            let status = match todo["status"].as_str().unwrap_or("") {
                "in_progress" => crate::protocol::TodoStatus::InProgress,
                "completed" => crate::protocol::TodoStatus::Completed,
                _ => crate::protocol::TodoStatus::Pending,
            };
            let priority = todo["priority"]
                .as_str()
                .map(str::to_ascii_lowercase)
                .filter(|p| matches!(p.as_str(), "high" | "medium" | "low"));
            entries.push(TodoEntry {
                content,
                status,
                priority,
                active_form: None,
            });
        }
        *ctx.todos.lock().unwrap() = entries;
        Ok("待办清单已更新".to_owned())
    })
}

const TODO_READ_DESCRIPTION: &str = "Reads the current task list.";

fn run_todo_read<'a>(
    ctx: &'a ToolContext,
    _args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let todos = ctx.todos.lock().unwrap().clone();
        if todos.is_empty() {
            return Ok("（当前无待办）".to_owned());
        }
        let lines: Vec<String> = todos
            .iter()
            .map(|entry| {
                format!(
                    "[{}] {}",
                    match entry.status {
                        crate::protocol::TodoStatus::Pending => "pending",
                        crate::protocol::TodoStatus::InProgress => "in_progress",
                        crate::protocol::TodoStatus::Completed => "completed",
                    },
                    entry.content
                )
            })
            .collect();
        Ok(lines.join("\n"))
    })
}

// ---------------------------------------------------------------------------
// TaskOutput / TaskStop
// ---------------------------------------------------------------------------

const TASK_OUTPUT_DESCRIPTION: &str = "Reads output from a background task by task_id.";

fn task_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "number", "description": "The background task id"}
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}

fn run_task_output<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let task_id = args["task_id"].as_u64().ok_or("缺少 task_id 参数")?;
        let task = ctx
            .background
            .lock()
            .unwrap()
            .get(&task_id)
            .cloned()
            .ok_or(format!("后台任务不存在: {task_id}"))?;
        let state = task.state.lock().unwrap();
        Ok(format!(
            "[{}] {}\n{}",
            if state.finished { "已完成" } else { "运行中" },
            state.exit_status.clone().unwrap_or_else(|| "running".to_owned()),
            truncate_output(&state.output)
        ))
    })
}

const TASK_STOP_DESCRIPTION: &str = "Stops a running background task by task_id.";

fn task_stop_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "number", "description": "The background task id"}
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}

fn run_task_stop<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let task_id = args["task_id"].as_u64().ok_or("缺少 task_id 参数")?;
        let task = ctx
            .background
            .lock()
            .unwrap()
            .get(&task_id)
            .cloned()
            .ok_or(format!("后台任务不存在: {task_id}"))?;
        let _ = task.kill.send(()).await;
        Ok(format!("已请求停止后台任务 {task_id}"))
    })
}

// ---------------------------------------------------------------------------
// Interaction tools (AskUserQuestion / plan mode)
// ---------------------------------------------------------------------------

const ASK_USER_QUESTION_DESCRIPTION: &str = "Asks the user 1-4 blocking questions with 2-4 options each. Use when you are blocked on a decision that is genuinely the user's to make. The user can always select \"Other\" to provide custom text input.";

fn ask_user_question_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "minLength": 1, "description": "The complete question to ask the user"},
                        "header": {"type": "string", "maxLength": 12, "description": "Very short label displayed as a chip/tag"},
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string", "description": "The display text for this option"},
                                    "description": {"type": "string", "description": "Explanation of what this option means or implies"}
                                },
                                "required": ["label", "description"],
                                "additionalProperties": false
                            }
                        },
                        "multiSelect": {"type": "boolean", "description": "Set to true to allow the user to select multiple options"}
                    },
                    // question carries the full text; header is only the chip label.
                    // The host always appends an "Other" free-text option.
                    "required": ["question", "options"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["questions"],
        "additionalProperties": false
    })
}

fn request_interaction<'a>(
    ctx: &'a ToolContext,
    kind: InteractionKind,
    payload: Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>> {
    Box::pin(async move {
        let id = format!("interact-{}", ctx.interact_counter.fetch_add(1, Ordering::Relaxed));
        let (responder_tx, responder_rx) = oneshot::channel();
        ctx.interact
            .send(InteractRequest {
                id,
                kind,
                payload,
                responder: responder_tx,
            })
            .await
            .map_err(|_| "交互通道已关闭".to_owned())?;
        responder_rx
            .await
            .map_err(|_| "交互请求已被取消".to_owned())
    })
}

fn run_ask_user_question<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let questions = args["questions"].as_array().ok_or("缺少 questions 参数")?;
        if questions.is_empty() || questions.len() > 4 {
            return Err("questions 数量须为 1-4".to_owned());
        }
        let answer =
            request_interaction(ctx, InteractionKind::Ask, args["questions"].clone()).await?;
        // The host returns {"answers": {question id → [labels]}}; surface a
        // compact human-readable summary for the model.
        match answer.get("answers") {
            Some(answers) => {
                let mut lines = Vec::new();
                if let Some(map) = answers.as_object() {
                    for (question, picks) in map {
                        lines.push(format!(
                            "{question}: {}",
                            picks
                                .as_array()
                                .map(|items| items
                                    .iter()
                                    .filter_map(|item| item.as_str())
                                    .collect::<Vec<_>>()
                                    .join("、"))
                                .unwrap_or_default()
                        ));
                    }
                }
                if lines.is_empty() {
                    Ok("用户跳过了提问（decline）".to_owned())
                } else {
                    Ok(lines.join("\n"))
                }
            }
            None => Ok("用户跳过了提问（decline）".to_owned()),
        }
    })
}

const ENTER_PLAN_MODE_DESCRIPTION: &str = "Switches into plan mode: read-only exploration, then present a plan via ExitPlanMode for user approval before implementing. Requires user approval.";

fn run_enter_plan_mode<'a>(
    ctx: &'a ToolContext,
    _args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let answer = request_interaction(ctx, InteractionKind::Plan, serde_json::json!({
            "action": "enter_plan"
        }))
        .await?;
        match answer.get("approved").and_then(Value::as_bool) {
            Some(true) => Ok("已进入计划模式。现在可以只读探索代码，完成后用 ExitPlanMode 提交实施计划。".to_owned()),
            _ => Ok("用户未批准进入计划模式".to_owned()),
        }
    })
}

const EXIT_PLAN_MODE_DESCRIPTION: &str = "Presents the final implementation plan for user approval. The plan parameter carries the complete plan content; the user sees it before approving.";

fn exit_plan_mode_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "plan": {"type": "string", "minLength": 1, "maxLength": 20000, "description": "The implementation plan to present for approval"}
        },
        "required": ["plan"],
        "additionalProperties": false
    })
}

fn run_exit_plan_mode<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let plan = args["plan"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("缺少 plan 参数")?
            .to_owned();
        let answer = request_interaction(
            ctx,
            InteractionKind::Plan,
            serde_json::json!({ "action": "exit_plan", "plan": plan }),
        )
        .await?;
        match answer.get("approved").and_then(Value::as_bool) {
            Some(true) => Ok("计划已批准。现在退出计划模式，按计划开始实施。".to_owned()),
            _ => Ok(
                "用户未批准该计划。请根据反馈修订计划（不要直接开始实施）。".to_owned(),
            ),
        }
    })
}

// ---------------------------------------------------------------------------
// Cron tools (host-bridged via AutomationStore)
// ---------------------------------------------------------------------------

const CRON_CREATE_DESCRIPTION: &str = "Creates a persistent scheduled automation for the current workspace. One schedule form: cron (5 fields, local timezone) OR delayMinutes (relative one-shot), OR intervalUnit+interval for every-N recurrence. The prompt must be fully self-contained.";

fn cron_create_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "cron": {"type": "string", "description": "Standard 5-field cron expression (minute hour dom month dow) in the user's local timezone; required unless delayMinutes is set"},
            "delayMinutes": {"type": "number", "exclusiveMinimum": 0, "maximum": 525600, "description": "Relative delay in whole minutes from now (one-shot)"},
            "prompt": {"type": "string", "minLength": 1, "description": "Complete instruction sent at every scheduled fire"},
            "title": {"type": "string", "minLength": 1, "description": "Concise title preserving the user's natural-language schedule phrase"},
            "recurring": {"type": "boolean", "description": "true (default) repeats indefinitely; false is finite"},
            "maxRuns": {"type": "number", "exclusiveMinimum": 0, "description": "Max successful dispatches; only with recurring=false (default 1)"},
            "intervalUnit": {"type": "string", "enum": ["minute", "hourly", "daily", "weekly", "monthly", "yearly"]},
            "interval": {"type": "number", "minimum": 1, "maximum": 200}
        },
        "required": ["prompt", "title"],
        "additionalProperties": false
    })
}

fn automation_store(ctx: &ToolContext) -> Result<Arc<dyn AutomationStore>, String> {
    ToolContext::automation_store(ctx)
        .ok_or_else(|| "定时任务后端未接入".to_owned())
}

fn run_cron_create<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let store = automation_store(ctx)?;
        let title = args["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("缺少 title 参数")?
            .to_owned();
        let prompt = args["prompt"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("缺少 prompt 参数")?
            .to_owned();
        let schedule = super::automation::ScheduleSpec::from_tool_args(args)?;
        store.create(title, prompt, schedule, args).await
    })
}

const CRON_LIST_DESCRIPTION: &str = "Lists the scheduled automations in the current workspace with status and schedule.";

fn run_cron_list<'a>(
    ctx: &'a ToolContext,
    _args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let store = automation_store(ctx)?;
        store.list().await
    })
}

const CRON_UPDATE_DESCRIPTION: &str = "Updates selected fields of an existing automation (id required; title required and rewritten to stay consistent).";

fn cron_update_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "Automation id"},
            "title": {"type": "string", "description": "Replacement title (required on every update)"},
            "cron": {"type": "string"},
            "prompt": {"type": "string"},
            "recurring": {"type": "boolean"},
            "maxRuns": {"type": "number", "exclusiveMinimum": 0}
        },
        "required": ["id", "title"],
        "additionalProperties": false
    })
}

fn run_cron_update<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let store = automation_store(ctx)?;
        let id = args["id"].as_str().ok_or("缺少 id 参数")?.to_owned();
        let title = args["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("缺少 title 参数（更新时必须同步改写标题）")?
            .to_owned();
        store.update(&id, title, args).await
    })
}

const CRON_DELETE_DESCRIPTION: &str = "Deletes a scheduled automation by id.";

fn cron_delete_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "Automation id"}
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

fn run_cron_delete<'a>(
    ctx: &'a ToolContext,
    args: &'a Value,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        let store = automation_store(ctx)?;
        let id = args["id"].as_str().ok_or("缺少 id 参数")?.to_owned();
        store.delete(&id).await
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matcher_handles_star_doublestar_and_components() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/a/main.rs"));
        assert!(glob_match("src/*.ts", "src/lib.ts"));
        assert!(!glob_match("src/*.ts", "src/lib/util.ts"));
        assert!(glob_match("**/*", "a/b/c.txt"));
        assert!(glob_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(!glob_match("src/**/*.rs", "other/a.rs"));
        assert!(glob_match("test?.txt", "test1.txt"));
        assert!(!glob_match("test?.txt", "test12.txt"));
        // Windows separators normalize.
        assert!(glob_match("src/*.ts", r"src\lib.ts"));
    }

    /// invocation 调用封装（cmd 方言）：剥掉模型自带的 `cmd /c` 前缀只留
    /// 一层包装（POSIX 方言的等价断言在 shell.rs）。
    #[cfg(windows)]
    #[test]
    fn invocation_strips_duplicate_cmd_prefix_for_cmd_dialect() {
        if crate::engine::shell::resolve().dialect != crate::engine::shell::ShellDialect::Cmd {
            return;
        }
        // 裸命令原样透传。
        assert_eq!(
            crate::engine::shell::invocation("echo 中文"),
            ("cmd".to_owned(), vec!["/c".to_owned(), "echo 中文".to_owned()])
        );
        // `cmd /c` / `CMD.EXE /C` 前缀剥掉，大小写不敏感。
        assert_eq!(
            crate::engine::shell::invocation("cmd /c echo 中文"),
            ("cmd".to_owned(), vec!["/c".to_owned(), "echo 中文".to_owned()])
        );
        assert_eq!(
            crate::engine::shell::invocation("CMD.EXE /C dir"),
            ("cmd".to_owned(), vec!["/c".to_owned(), "dir".to_owned()])
        );
        // 多余空白与内层引号保留。
        assert_eq!(
            crate::engine::shell::invocation("  cmd /c   echo \"a b\""),
            ("cmd".to_owned(), vec!["/c".to_owned(), "echo \"a b\"".to_owned()])
        );
        // /c 与 cmd 无关不剥（如 `cmd /list`），保持原样包装。
        assert_eq!(
            crate::engine::shell::invocation("cmd /list"),
            ("cmd".to_owned(), vec!["/c".to_owned(), "cmd /list".to_owned()])
        );
        // cmdkey 等以 cmd 开头的程序不被剥。
        assert_eq!(
            crate::engine::shell::invocation("cmdkey /list"),
            ("cmd".to_owned(), vec!["/c".to_owned(), "cmdkey /list".to_owned()])
        );
    }

    #[tokio::test]
    async fn write_read_edit_round_trip_and_guards() {
        let dir = std::env::temp_dir().join(format!("channel-engine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (interact_tx, _interact_rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&dir, "sess-test".to_owned(), interact_tx);
        let path = dir.join("note.txt");
        // A file that predates the session (not written through the tool).
        let existing = dir.join("existing.txt");
        std::fs::write(&existing, "old").unwrap();

        // Overwriting a pre-existing file without a prior read fails.
        let result = run_write(
            &ctx,
            &serde_json::json!({"file_path": existing.to_string_lossy(), "content": "nope"}),
        )
        .await;
        assert!(result.is_err(), "未先覆写必须拒绝: {result:?}");

        // Write fresh (no prior read needed for a new file).
        let result = run_write(
            &ctx,
            &serde_json::json!({"file_path": path.to_string_lossy(), "content": "alpha\nbeta\n"}),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");

        // Edit without a prior read fails (pre-existing file, never read).
        let result = run_edit(
            &ctx,
            &serde_json::json!({
                "file_path": existing.to_string_lossy(),
                "old_string": "old",
                "new_string": "new"
            }),
        )
        .await;
        assert!(result.is_err(), "未先编辑必须拒绝: {result:?}");

        // Read registers the file.
        let result = run_read(&ctx, &serde_json::json!({"file_path": path.to_string_lossy()})).await;
        let text = result.unwrap();
        assert!(text.starts_with("1\talpha\n2\tbeta"), "{text}");

        // Edit uniqueness guard.
        let result = run_edit(
            &ctx,
            &serde_json::json!({
                "file_path": path.to_string_lossy(),
                "old_string": "a",
                "new_string": "X"
            }),
        )
        .await;
        assert!(result.is_err(), "非唯一 old_string 必须拒绝: {result:?}");

        // Successful edit.
        let result = run_edit(
            &ctx,
            &serde_json::json!({
                "file_path": path.to_string_lossy(),
                "old_string": "alpha",
                "new_string": "ALPHA"
            }),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "ALPHA\nbeta\n");

        // Replace_all.
        std::fs::write(&path, "x y x").unwrap();
        let result = run_edit(
            &ctx,
            &serde_json::json!({
                "file_path": path.to_string_lossy(),
                "old_string": "x",
                "new_string": "z",
                "replace_all": true
            }),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "z y z");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bash_runs_and_reports_failure() {
        let dir = std::env::temp_dir();
        let (interact_tx, _interact_rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&dir, "sess-test".to_owned(), interact_tx);
        if cfg!(windows) {
            let result = run_bash(&ctx, &serde_json::json!({"command": "echo hello"})).await;
            assert_eq!(result.unwrap().trim(), "hello");
            let result = run_bash(
                &ctx,
                &serde_json::json!({"command": "exit 3"}),
            )
            .await;
            assert!(result.is_err(), "非零退出且无输出必须报错");
        } else {
            let result = run_bash(&ctx, &serde_json::json!({"command": "echo hello"})).await;
            assert_eq!(result.unwrap().trim(), "hello");
        }
    }

    #[tokio::test]
    async fn todo_write_read_round_trip() {
        let dir = std::env::temp_dir();
        let (interact_tx, _interact_rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&dir, "sess-test".to_owned(), interact_tx);
        run_todo_write(
            &ctx,
            &serde_json::json!({"todos": [
                {"content": "a", "status": "in_progress", "priority": "high"},
                {"content": "b", "status": "pending", "priority": "low"}
            ]}),
        )
        .await
        .unwrap();
        let text = run_todo_read(&ctx, &serde_json::json!({})).await.unwrap();
        assert!(text.contains("[in_progress] a"));
        assert!(text.contains("[pending] b"));
    }

    /// POSIX 方言真机护栏（Windows Git Bash / Unix shell）：Unix 管道与
    /// 引号语义按 POSIX 生效——这是模型一次写出合规网络请求的前提
    /// （cmd 方言无 sed/tr，自动跳过）。2026-11-07 网络请求连环报障回归：
    /// 修复前模型写 POSIX 语法被 cmd /c 咬碎（head 不存在、单引号成主机
    /// 名），修复后 Git Bash 探测命中即 POSIX 语义直达。
    #[tokio::test]
    async fn bash_posix_pipeline_runs_when_dialect_is_posix() {
        let shell = crate::engine::shell::resolve();
        if !matches!(
            shell.dialect,
            crate::engine::shell::ShellDialect::GitBash | crate::engine::shell::ShellDialect::Posix
        ) {
            return;
        }
        let dir = std::env::temp_dir();
        let (interact_tx, _interact_rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&dir, "sess-test".to_owned(), interact_tx);
        let result = run_bash(&ctx, &serde_json::json!({"command": "echo \"hello world\" | tr h H | sed 's/world/posix/'"})).await;
        assert_eq!(result.unwrap().trim(), "Hello posix");
    }

    /// cwd 持久化（POSIX 方言）：cd 进子目录后，下一次 Bash 调用的工作
    /// 目录应落在那里（ZCode cwdFilePath 语义）；Read 等工具的相对路径
    /// 解析也随之跟随。cmd 方言不支持包装，跳过。
    #[tokio::test]
    async fn bash_cwd_persists_across_calls_for_posix() {
        let shell = crate::engine::shell::resolve();
        if !matches!(
            shell.dialect,
            crate::engine::shell::ShellDialect::GitBash | crate::engine::shell::ShellDialect::Posix
        ) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ba-cwd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (interact_tx, _interact_rx) = mpsc::channel(8);
        let ctx = ToolContext::new(&dir, "sess-cwd".to_owned(), interact_tx);
        let target = if cfg!(windows) {
            format!("cd \"{}\"", dir.join("sub").to_string_lossy().replace('\\', "/"))
        } else {
            format!("cd \"{}\"", dir.join("sub").display())
        };
        run_bash(&ctx, &serde_json::json!({ "command": target })).await.unwrap();
        let result = run_bash(&ctx, &serde_json::json!({"command": "pwd"})).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let reported = result.trim();
        let here = ctx.cwd.lock().unwrap().display().to_string();
        assert!(here.replace('\\', "/").ends_with("sub") || reported.ends_with("sub"));
    }
}
