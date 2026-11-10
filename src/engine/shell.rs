//! Bash-tool shell resolution — the ZCode Windows scheme, ported.
//!
//! The Bash tool executes model-authored commands through a resolved
//! shell. ZCode's actual scheme (extracted from the engine bundle,
//! `resolveEffectiveBashShellSelection` + the `# Environment` context
//! section) is:
//!
//! 1. Windows prefers Git Bash: static `C:\Program Files[ (x86)]\Git\bin
//!    \bash.exe` probes first, then `git` resolved from PATH with
//!    `<dir>/../bin/bash.exe` and `<dir>/../../bin/bash.exe` siblings.
//!    Found → commands run via `bash.exe -c` (POSIX dialect): quoting,
//!    pipes and the Unix toolset (head/grep/sed/curl …) behave as the
//!    model expects, which is what makes one-shot network requests work.
//! 2. No Git Bash → legacy fallback: the system shell (`cmd /c`).
//!    (WSL `System32\bash.exe` is deliberately never picked — ZCode only
//!    probes the two static paths and git siblings.)
//! 3. Whatever resolved is disclosed to the model: an `# Environment`
//!    block (Platform / Shell / OS Version / cwd / git status) rides the
//!    system prompt, and the Bash tool description states the active
//!    dialect, so the model writes commands for the shell that will
//!    actually run them.
//!
//! `resolve()` caches the probe result for the process lifetime (fs
//! probes, a PATH scan and one `ver` spawn run once). The module lives
//! under the `engine` feature alongside its consumers.

use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::OnceLock;

/// The dialect a resolved shell speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    /// Git Bash on Windows (`bash.exe -c`, POSIX semantics).
    GitBash,
    /// A POSIX shell on Unix (`$SHELL`/`sh -c`).
    Posix,
    /// Windows system-shell fallback (`cmd /c`, legacy-shell semantics).
    Cmd,
}

/// The shell the Bash tool executes through.
#[derive(Debug, Clone)]
pub struct ShellSelection {
    pub dialect: ShellDialect,
    /// Human/model-facing name (the `# Environment` `Shell:` value).
    pub display: String,
    /// Program the commands are spawned through.
    pub program: String,
}

/// Resolves (once per process) the shell the Bash tool executes through.
pub fn resolve() -> &'static ShellSelection {
    static SHELL: OnceLock<ShellSelection> = OnceLock::new();
    SHELL.get_or_init(detect)
}

fn detect() -> ShellSelection {
    #[cfg(windows)]
    {
        if let Some(bash) = detect_windows_git_bash() {
            return ShellSelection {
                dialect: ShellDialect::GitBash,
                display: format!("Git Bash ({})", bash.display()),
                program: bash.to_string_lossy().into_owned(),
            };
        }
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_owned());
        let display = Path::new(&comspec)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "cmd.exe".to_owned());
        ShellSelection {
            dialect: ShellDialect::Cmd,
            display,
            program: "cmd".to_owned(),
        }
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
        let display = Path::new(&shell)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| shell.clone());
        ShellSelection {
            dialect: ShellDialect::Posix,
            display: display.clone(),
            program: if display == "sh" { "sh".to_owned() } else { shell },
        }
    }
}

/// ZCode's static Git Bash candidates, probed in order.
#[cfg(windows)]
const GIT_BASH_STATIC: &[&str] = &[
    r"C:\Program Files\Git\bin\bash.exe",
    r"C:\Program Files (x86)\Git\bin\bash.exe",
];

/// Windows Git Bash auto-detection (ZCode `resolveWindowsGitBashShell`
/// parity): static candidates first, then `git` from PATH with the
/// `<dir>/../bin/bash.exe` and `<dir>/../../bin/bash.exe` siblings.
#[cfg(windows)]
fn detect_windows_git_bash() -> Option<PathBuf> {
    for candidate in GIT_BASH_STATIC {
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for git in git_on_path()? {
        let git_dir = git.parent()?.to_path_buf();
        let mut candidates = vec![git_dir.join(r"..\bin\bash.exe")];
        if let Some(parent) = git_dir.parent() {
            candidates.push(parent.join(r"..\bin\bash.exe"));
        }
        for candidate in candidates {
            if candidate.is_file() {
                return Some(normalize_windows_path(&candidate));
            }
        }
    }
    None
}

/// Scans PATH for `git.exe` (ZCode probes the `git` launcher the same way;
/// a PATH scan keeps this dependency-free). Returns every hit in order.
#[cfg(windows)]
fn git_on_path() -> Option<Vec<PathBuf>> {
    let path = std::env::var("PATH").ok()?;
    let hits: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("git.exe"))
        .filter(|candidate| candidate.is_file())
        .collect();
    (!hits.is_empty()).then_some(hits)
}

/// Collapses `..` components lexically (spawn tolerates them, but the
/// path shows up in the env disclosure and sidecar files).
#[cfg(windows)]
fn normalize_windows_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Builds the spawn invocation for a Bash-tool command: POSIX dialects run
/// `bash/sh -c <command>`; the cmd fallback keeps the legacy wrapper (and
/// strips a model-authored duplicate `cmd /c` prefix — cmd's /c quote
/// rules leak one trailing quote into the command when the prefixes
/// stack, so only one layer is kept).
pub fn invocation(command: &str) -> (String, Vec<String>) {
    match resolve().dialect {
        ShellDialect::GitBash | ShellDialect::Posix => {
            (resolve().program.clone(), vec!["-c".to_owned(), command.to_owned()])
        }
        ShellDialect::Cmd => {
            let mut rest = command.trim_start();
            for prefix in ["cmd.exe", "cmd"] {
                let matched = rest.len() > prefix.len()
                    && rest.as_bytes()[..prefix.len()]
                        .eq_ignore_ascii_case(prefix.as_bytes())
                    && rest.as_bytes()[prefix.len()].is_ascii_whitespace();
                if !matched {
                    continue;
                }
                let after = rest[prefix.len()..].trim_start_matches([' ', '\t']);
                let bytes = after.as_bytes();
                if bytes.len() >= 2
                    && bytes[0] == b'/'
                    && (bytes[1] == b'c' || bytes[1] == b'C')
                {
                    rest = after[2..].trim_start();
                }
                break;
            }
            ("cmd".to_owned(), vec!["/c".to_owned(), rest.to_owned()])
        }
    }
}

/// The Bash tool description, dialect-aware (ZCode states the shell in
/// the tool text — the model must know which grammar to write).
pub fn bash_description() -> String {
    let base = "Executes a shell command and returns its output. Set run_in_background to run detached from the turn; you will be notified on exit and can read output with TaskOutput.";
    match resolve().dialect {
        ShellDialect::GitBash => format!(
            "{base} Commands run in Git Bash (POSIX): the Unix toolset (head, tail, grep, sed, awk, curl, ...), pipes and standard quoting all work; Windows paths (C:\\foo) are accepted. Working directory persists between calls (shell state like env vars does not)."
        ),
        ShellDialect::Cmd => format!(
            "{base} Commands run in the Windows Command Prompt (cmd.exe): POSIX tools (head, tail, grep, sed, awk) do not exist - use findstr, more, or `powershell -NoProfile -Command \"...\"`; quote with double quotes only (single quotes are not quoting); backslash line continuations and multiline commands do not work. Working directory does not persist between calls."
        ),
        ShellDialect::Posix => format!(
            "{base} Commands run in the user's POSIX shell: standard quoting, pipes and the Unix toolset apply. Working directory persists between calls (shell state like env vars does not)."
        ),
    }
}

/// The `# Environment` block (ZCode context semantics): platform / shell /
/// OS disclosure plus one dialect guidance paragraph, appended to the
/// system prompt of every new session (main loop and subagents alike).
pub fn environment_section(workspace: &Path) -> String {
    let shell = resolve();
    let platform = if cfg!(windows) { "win32" } else { std::env::consts::OS };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    // Compile-time gates: `windows_os_version()` only exists on Windows,
    // so a runtime `if cfg!(windows)` would not compile on other targets.
    #[cfg(windows)]
    let os_version = format!("win32 {} {}", windows_os_version(), arch);
    #[cfg(not(windows))]
    let os_version = format!("{} {}", std::env::consts::OS, arch);
    let guidance = match shell.dialect {
        ShellDialect::GitBash => "The Bash tool executes commands with Git Bash (POSIX): quoting, pipes and the Unix toolset work as documented, so multi-step network requests (curl, ...) succeed as one command. Windows paths are accepted; inside the shell they also read as /c/... style paths.",
        ShellDialect::Cmd => "The Bash tool executes commands with cmd.exe: POSIX tools (head, tail, grep, sed, awk) are not available (use findstr, more, or PowerShell), single quotes do not quote (use double quotes), and backslash line continuations do not work. For web requests use curl with double-quoted URLs on a single line.",
        ShellDialect::Posix => "The Bash tool executes commands with the user's POSIX shell: standard quoting, pipes and the Unix toolset apply.",
    };
    format!(
        "# Environment\n\
         - Primary working directory: {}\n\
         - Is a git repository: {}\n\
         - Platform: {}\n\
         - Shell: {}\n\
         - OS Version: {}\n\n{}",
        workspace.display(),
        workspace.join(".git").exists(),
        platform,
        shell.display,
        os_version,
        guidance,
    )
}

/// Windows release string (ZCode `os.release()` shape, e.g.
/// "10.0.26300.6475") parsed once from `cmd /c ver`; "windows" fallback.
#[cfg(windows)]
fn windows_os_version() -> String {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            use std::os::windows::process::CommandExt;
            let output = std::process::Command::new("cmd")
                .args(["/c", "ver"])
                .creation_flags(0x0800_0000)
                .output();
            let text = output
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            let digits: String = text.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
            let trimmed = digits.trim_matches('.');
            if trimmed.split('.').all(|part| !part.is_empty()) && !trimmed.is_empty() {
                trimmed.to_owned()
            } else {
                "windows".to_owned()
            }
        })
        .clone()
}

/// Converts a POSIX-style path reported by Git Bash (`/e/foo`, `/mnt/e/foo`)
/// into a Windows path; `None` when the path does not carry a drive mapping
/// (the caller keeps the raw value then).
#[cfg(windows)]
pub(crate) fn posix_to_windows_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let (mount, rest) = trimmed
        .strip_prefix("/mnt/")
        .map(|rest| (true, rest))
        .unwrap_or((false, trimmed.strip_prefix('/')?));
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    let tail = parts.next().unwrap_or("");
    let mut path = if drive.len() == 1
        && drive.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
    {
        // Seed with the root (`E:\`, not bare `E:`): a bare prefix makes
        // PathBuf::push append drive-relative (`E:foo`) and is_dir() then misses.
        PathBuf::from(format!("{}:\\", drive.to_ascii_uppercase()))
    } else if drive == "tmp" && !mount {
        // Git for Windows mounts /tmp over the Windows user temp dir
        // (`none /tmp usertemp` in its fstab), so a bash whose working
        // directory lives under %TEMP% reports `/tmp/...`; map it back.
        std::env::temp_dir()
    } else {
        return None;
    };
    push_posix_tail(&mut path, tail);
    Some(path)
}

/// Appends a POSIX tail (`a/b/../c`) to a Windows path, honoring `.`/`..`.
#[cfg(windows)]
fn push_posix_tail(path: &mut PathBuf, tail: &str) {
    for segment in tail.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            path.pop();
        } else {
            path.push(segment);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 真机形态：装了 Git 的开发机应解析出 Git Bash（静态路径或
    /// git 反推），且 program 指向真实存在的 bash.exe；没装 Git 时回落
    /// cmd（本断言按探测结果双向自洽，CI 无 Git 也成立）。
    #[test]
    #[cfg(windows)]
    fn resolve_matches_installed_git() {
        let shell = resolve();
        let git_bash = detect_windows_git_bash();
        if git_bash.is_some() {
            assert_eq!(shell.dialect, ShellDialect::GitBash);
            assert!(Path::new(&shell.program).is_file(), "program 应存在: {}", shell.program);
            assert!(shell.display.contains("Git Bash"));
        } else {
            assert_eq!(shell.dialect, ShellDialect::Cmd);
        }
    }

    /// Unix 真机形态：Posix 方言 + program 可执行。
    #[test]
    #[cfg(not(windows))]
    fn resolve_is_posix_on_unix() {
        let shell = resolve();
        assert_eq!(shell.dialect, ShellDialect::Posix);
        assert!(!shell.program.is_empty());
    }

    /// 调用封装：POSIX 方言一律 `bash/sh -c <command>` 单串透传。
    #[test]
    fn invocation_passes_single_string_for_posix() {
        if resolve().dialect != ShellDialect::GitBash && resolve().dialect != ShellDialect::Posix {
            return;
        }
        let (program, args) = invocation("curl \"https://example.com\" | head");
        assert_eq!(args, vec!["-c".to_owned(), "curl \"https://example.com\" | head".to_owned()]);
        assert!(!program.is_empty());
    }

    /// 调用封装：cmd 方言剥掉模型自带的 cmd /c 前缀，只留一层包装。
    #[test]
    #[cfg(windows)]
    fn invocation_strips_duplicated_cmd_prefix() {
        let (program, args) = invocation("cmd /c echo hello");
        if resolve().dialect == ShellDialect::Cmd {
            assert_eq!(program, "cmd");
            assert_eq!(args, vec!["/c".to_owned(), "echo hello".to_owned()]);
        }
    }

    /// 环境段：含 ZCode 风格五要素与工作目录、方言指引。
    #[test]
    fn environment_section_discloses_platform_and_shell() {
        let dir = std::env::temp_dir();
        let section = environment_section(&dir);
        assert!(section.starts_with("# Environment"));
        assert!(section.contains(&format!("- Primary working directory: {}", dir.display())));
        assert!(section.contains("- Is a git repository: "));
        assert!(section.contains(if cfg!(windows) { "- Platform: win32" } else { "- Platform: " }));
        assert!(section.contains("- Shell: "));
        assert!(section.contains("- OS Version: "));
        assert!(section.contains("The Bash tool"));
    }

    /// Windows 版本号解析：`cmd /c ver` 应产出形如 10.0.xxxxx 的串。
    #[test]
    #[cfg(windows)]
    fn windows_os_version_parses_release() {
        let version = windows_os_version();
        assert!(
            version == "windows" || version.split('.').count() >= 3,
            "版本串异常: {version}"
        );
    }

    /// POSIX 路径反解：Git Bash pwd 形态与 WSL /mnt 形态。
    #[test]
    #[cfg(windows)]
    fn posix_to_windows_path_maps_drives() {
        assert_eq!(
            posix_to_windows_path("/e/GitHub_fa/BlackAgent"),
            Some(PathBuf::from(r"E:\GitHub_fa\BlackAgent"))
        );
        assert_eq!(
            posix_to_windows_path("/mnt/c/Users"),
            Some(PathBuf::from(r"C:\Users"))
        );
        assert_eq!(posix_to_windows_path("/not-a-drive"), None);
        assert_eq!(posix_to_windows_path("C:\\already\\windows"), None);
        // Git for Windows 的 usertemp 挂载：bash 起始目录在 %TEMP% 下时
        // pwd 会报 `/tmp/...`，反解回 Windows 临时目录。
        assert_eq!(
            posix_to_windows_path("/tmp/ba-cwd-1/sub"),
            Some(std::env::temp_dir().join("ba-cwd-1").join("sub"))
        );
    }
}
