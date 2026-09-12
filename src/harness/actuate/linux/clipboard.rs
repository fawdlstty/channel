//! Linux 剪贴板粘贴（upgrade.md §7.1，P1 实现）。
//!
//! 两个职责：
//!
//! 1. **工具探测**：`xclip`（X11）/ `wl-copy`+`wl-paste`（wl-clipboard，
//!    Wayland）按显示服务线索选主用、另一个作次选（XWayland 会话两套都
//!    可能装了），PATH 搜索 + 执行位判定，零 shell 调用；都缺失 →
//!    [`Error::ClipboardUnavailable`]（code `clipboard-unavailable`）诚实降级；
//! 2. **平台无关善后骨架** [`type_text_via_clipboard`]：抽自 sendinput.rs
//!    Windows 实现的粘贴协议——副本 → 置入（失败也先恢复副本再返回原错误）
//!    → settle 等待 → `paste()` → consume 等待 → best-effort 恢复副本。
//!    IO 面经 [`ClipboardIo`] 注入：Windows 的 OpenClipboard/GlobalLock 族
//!    与本文件的 xclip/wl-copy 子进程都归一到「读/写文本」两个动作，
//!    协议时序只有一份实现（单测以 mock io 锁定调用序列）。
//!
//! 隐私取舍与 sendinput.rs 同款（协议级已知）：键入文本短暂进入系统剪贴板
//! （可能被剪贴板历史/同步器留存）；恢复只还原文本格式；VcXsrv 等远程
//! 剪贴板同步的竞态经加大 `consume_ms` 缓解（§7.5-5，todo 跟踪实测）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use crate::harness::Error;

/// 探测到的剪贴板工具（运行时产物；探测缺失 → None → 上层报
/// [`Error::ClipboardUnavailable`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardTool {
    /// xclip（X11；读写同一二进制：`-selection clipboard [-o]`）。
    Xclip { path: PathBuf },
    /// wl-clipboard（Wayland；copy 置入、paste 读出——paste 缺席只影响
    /// 副本恢复这个善后动作，不影响粘贴主流程）。
    Wl { copy: PathBuf, paste: Option<PathBuf> },
}

/// 显示服务线索 → 探测顺序（纯函数，单测锁定）：Wayland 会话 wl-clipboard
/// 优先、X11（含 XWayland）xclip 优先。
fn probe_order(wayland: bool) -> [&'static str; 2] {
    if wayland { ["wl-copy", "xclip"] } else { ["xclip", "wl-copy"] }
}

/// 运行时探测剪贴板工具（PATH 搜索；均缺失 → None）。
pub fn detect_clipboard_tool() -> Option<ClipboardTool> {
    // wayland 线索复用 detect_backends（探测语义单一来源；与 feature 无关）。
    let wayland = crate::harness::snapshot::linux::detect_backends().wayland;
    let [first, second] = probe_order(wayland);
    for name in [first, second] {
        match (name, which(name)) {
            ("xclip", Some(path)) => return Some(ClipboardTool::Xclip { path }),
            ("wl-copy", Some(copy)) => {
                return Some(ClipboardTool::Wl { copy, paste: which("wl-paste") })
            }
            _ => continue,
        }
    }
    None
}

/// PATH 搜索可执行文件（env 包装；探测逻辑核心在 [`which_in`] 供单测）。
fn which(name: &str) -> Option<PathBuf> {
    let paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    which_in(name, &paths)
}

/// 在给定目录清单里找可执行文件（纯函数，单测锁定）：常规文件 + 任一执行
/// 位（mode & 0o111）。按 mode 位判定——noexec 挂载（如 /tmp）不影响
/// access 语义的探测可测性，实际 exec 失败由后续 spawn 阶段诚实报错。
fn which_in(name: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().map(|dir| dir.join(name)).find(|cand| is_executable_file(cand))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/* --- 平台无关善后协议（§7.1） ----------------------------------------------- */

/// 善后协议的剪贴板 IO 面：Windows（sendinput.rs 的 Win32 剪贴板 API 族）
/// 与 Linux（xclip/wl-copy 子进程）在此归一，协议本体 [`type_text_via_clipboard`]
/// 只依赖这两个动作。
pub trait ClipboardIo {
    /// 读当前剪贴板文本（留副本用）。失败/无文本一律 `None`——副本是善后
    /// 优化而非功能本体，读不到就跳过恢复（sendinput.rs 同语义）。
    fn read_text(&self) -> Option<String>;
    /// 置入文本。失败返回 Err（骨架负责先恢复副本再返回原错误）。
    fn write_text(&self, text: &str) -> Result<(), Error>;
}

/// 粘贴善后协议（upgrade.md §7.1；sendinput.rs Windows 实现的抽象骨架）：
///
/// 1. `io.read_text()` 留副本（失败照常继续——功能优先，宁可丢剪贴板恢复
///    也不丢键入）；
/// 2. `io.write_text(text)` 置入；**失败也先恢复副本**再返回原错误（置入
///    失败时剪贴板可能已被清空，不能提前 `?` 丢弃恢复动作——P2-7 语义）；
/// 3. 等 `settle_ms`（置入就绪 + 前台应用收到更新的最小时序）；
/// 4. `paste()` 触发粘贴（平台侧注入 ctrl+v / 等价键）；
/// 5. 等 `consume_ms`（留足目标应用消费粘贴的时间窗，避免恢复动作抢跑
///    把原文本粘贴进目标——慢应用/远程桌面可加大）；
/// 6. best-effort 恢复副本（失败忽略；只还原文本格式，其他剪贴板格式
///    无法复原——协议级已知取舍）。`paste()` 的错误保留为返回值。
pub fn type_text_via_clipboard(
    io: &dyn ClipboardIo,
    paste: &mut dyn FnMut() -> Result<(), Error>,
    text: &str,
    settle_ms: u64,
    consume_ms: u64,
) -> Result<(), Error> {
    let previous = io.read_text();
    if let Err(error) = io.write_text(text) {
        if let Some(previous) = previous.as_deref() {
            let _ = io.write_text(previous);
        }
        return Err(error);
    }
    sleep(Duration::from_millis(settle_ms));
    let result = paste();
    sleep(Duration::from_millis(consume_ms));
    if let Some(previous) = previous.as_deref() {
        let _ = io.write_text(previous);
    }
    result
}

/* --- xclip / wl-clipboard 子进程实例化 -------------------------------------- */

/// 命令行工具形态的 [`ClipboardIo`]（X11 用 xclip、Wayland 用 wl-clipboard）。
pub struct CommandClipboardIo {
    tool: ClipboardTool,
}

impl CommandClipboardIo {
    /// 以探测到的工具构造（[`detect_clipboard_tool`] 的产物）。
    pub fn new(tool: ClipboardTool) -> Self {
        Self { tool }
    }
}

impl ClipboardIo for CommandClipboardIo {
    fn read_text(&self) -> Option<String> {
        match &self.tool {
            ClipboardTool::Xclip { path } => {
                run_capture(path, &["-selection", "clipboard", "-o"])
            }
            // wl-paste -n：不加尾部换行（副本保真）；paste 缺席 → None（跳过恢复）。
            ClipboardTool::Wl { paste: Some(paste), .. } => run_capture(paste, &["-n"]),
            ClipboardTool::Wl { paste: None, .. } => None,
        }
    }

    fn write_text(&self, text: &str) -> Result<(), Error> {
        match &self.tool {
            // xclip 读 stdin 后自行 fork 持有 selection（后台常驻直至被替换），
            // 本进程 stdin 写完即回收子进程句柄。
            ClipboardTool::Xclip { path } => {
                run_feed(path, &["-selection", "clipboard"], text, "xclip")
            }
            ClipboardTool::Wl { copy, .. } => run_feed(copy, &[], text, "wl-copy"),
        }
    }
}

/// 子进程读剪贴板文本（stdout 捕获；任一失败 → None，副本路径不报错）。
fn run_capture(program: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// 子进程写剪贴板（stdin 喂入；spawn/写入/非零退出逐段归因）。
fn run_feed(program: &Path, args: &[&str], text: &str, tool: &'static str) -> Result<(), Error> {
    let fail =
        |detail: String| Error::ClipboardCommandFailed { tool, detail };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| fail(format!("spawn {program:?} 失败: {error}")))?;
    // stdin 写入失败（子进程早退/管道破裂）按置入失败处理；先 wait 收尸。
    if let Err(error) = child.stdin.take().unwrap().write_all(text.as_bytes()) {
        let _ = child.wait();
        return Err(fail(format!("stdin 写入失败: {error}")));
    }
    let status = child.wait().map_err(|error| fail(format!("wait 失败: {error}")))?;
    if !status.success() {
        return Err(fail(format!("退出码 {:?}", status.code())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /* --- 探测纯逻辑 -------------------------------------------------------- */

    #[test]
    fn probe_order_prefers_matching_display_service() {
        assert_eq!(probe_order(true), ["wl-copy", "xclip"]);
        assert_eq!(probe_order(false), ["xclip", "wl-copy"]);
    }

    #[test]
    fn which_in_finds_executable_by_mode_bits() {
        let dir = std::env::temp_dir().join(format!("harness-which-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fake-tool");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        make_executable(&exe);
        let plain = dir.join("plain-file");
        std::fs::write(&plain, b"data").unwrap();
        // 命中：执行位在；不命中：无执行位/不存在/别的名字。
        assert_eq!(which_in("fake-tool", &[dir.clone()]), Some(exe.clone()));
        assert_eq!(which_in("plain-file", &[dir.clone()]), None);
        assert_eq!(which_in("absent", &[dir.clone()]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// 本机探测：xclip/wl-copy 均未安装 → None（环境契约锁定——装上工具后
    /// 该测试转为记录实况，探测主逻辑已由 which_in 单测覆盖）。
    #[test]
    fn detect_returns_none_when_no_tool_installed() {
        if which("xclip").is_some() || which("wl-copy").is_some() {
            return; // 工具在场：跳过「缺失」断言（安装态由实机矩阵覆盖）。
        }
        assert_eq!(detect_clipboard_tool(), None);
    }

    /* --- 善后协议（mock io 锁定时序） -------------------------------------- */

    /// mock io：预置读值/写结果，动作序列经 RefCell 记录（write_text 按
    /// trait 取 `&self`）；settle/consume 传 0 保测试速度。
    struct MockIo {
        read_result: Option<String>,
        write_results: RefCell<VecDeque<Result<(), &'static str>>>,
        log: RefCell<Vec<String>>,
    }

    impl MockIo {
        fn new(read_result: Option<String>, writes: Vec<Result<(), &'static str>>) -> Self {
            Self {
                read_result,
                write_results: RefCell::new(writes.into_iter().collect()),
                log: RefCell::new(Vec::new()),
            }
        }

        fn actions(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
    }

    impl ClipboardIo for MockIo {
        fn read_text(&self) -> Option<String> {
            self.read_result.clone()
        }
        fn write_text(&self, text: &str) -> Result<(), Error> {
            self.log.borrow_mut().push(format!("write({text})"));
            match self.write_results.borrow_mut().pop_front() {
                Some(Ok(())) | None => Ok(()),
                Some(Err(reason)) => Err(Error::ClipboardCommandFailed {
                    tool: "mock",
                    detail: reason.to_string(),
                }),
            }
        }
    }

    #[test]
    fn happy_path_writes_pastes_then_restores() {
        let io = MockIo::new(Some("旧文本".into()), vec![Ok(()), Ok(())]);
        let mut pasted = 0;
        let result = type_text_via_clipboard(&io, &mut || {
            pasted += 1;
            Ok(())
        }, "新文本", 0, 0);
        assert!(result.is_ok());
        assert_eq!(pasted, 1, "粘贴恰好一次");
        assert_eq!(
            io.actions(),
            vec!["write(新文本)".to_string(), "write(旧文本)".to_string()],
            "置入 → 粘贴 → 恢复副本"
        );
    }

    #[test]
    fn no_backup_means_no_restore() {
        let io = MockIo::new(None, vec![Ok(())]);
        let result = type_text_via_clipboard(&io, &mut || Ok(()), "x", 0, 0);
        assert!(result.is_ok());
        assert_eq!(io.actions(), vec!["write(x)".to_string()], "无副本 → 只置入一次");
    }

    #[test]
    fn set_failure_restores_backup_and_returns_original_error() {
        let io = MockIo::new(Some("旧文本".into()), vec![Err("置入炸了"), Ok(())]);
        let mut pasted = false;
        let error = type_text_via_clipboard(&io, &mut || {
            pasted = true;
            Ok(())
        }, "新", 0, 0)
            .unwrap_err();
        assert!(!pasted, "置入失败不得触发粘贴");
        assert_eq!(error.to_string(), "剪贴板读写失败（mock: 置入炸了）");
        let actions = io.actions();
        assert_eq!(actions.len(), 2, "失败后仍恢复副本");
        assert_eq!(actions[1], "write(旧文本)");
    }

    #[test]
    fn paste_error_propagates_but_restore_still_runs() {
        let io = MockIo::new(Some("旧".into()), vec![Ok(()), Ok(())]);
        let result = type_text_via_clipboard(
            &io,
            &mut || Err(Error::XTestUnavailable),
            "新",
            0,
            0,
        );
        assert!(matches!(result, Err(Error::XTestUnavailable)), "粘贴错误保留");
        assert_eq!(io.actions().len(), 2, "善后不丢：粘贴失败也恢复副本");
    }
}
