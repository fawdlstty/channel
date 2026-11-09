//! Engine permission gate — ZCode five-mode semantics
//! (`plan` / `build` / `edit` / `yolo` / `guarded`, plus legacy aliases).
//!
//! Tool classes:
//! - read-only tools (`Read`/`Glob`/`Grep`/`TodoRead`/...) run in every mode;
//! - `plan` mode rejects everything else outright (read-only planning);
//! - `build` asks for a permission on every write-class tool;
//! - `edit` auto-approves file edits (`Write`/`Edit`), still asks for shell;
//! - `yolo` runs everything;
//! - `guarded` runs everything but pauses once on explicitly recognized
//!   dangerous shell commands (rm -rf, git reset --hard, force push,
//!   rsync --delete, ...).

use std::fmt;

/// ZCode permission modes. Legacy UI aliases (`confirm`/`autoEdit`/`full`)
/// map onto `build`/`edit`/`yolo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermMode {
    Plan,
    Build,
    Edit,
    Yolo,
    Guarded,
}

impl PermMode {
    /// Parses the mode string handed down with the turn. Unknown values
    /// fall back to [`PermMode::Build`] (ask-before-changes), matching the
    /// ZCode default for standard (non-headless) sessions.
    pub fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or_default().trim().to_ascii_lowercase().as_str() {
            "plan" => Self::Plan,
            "edit" | "autoedit" => Self::Edit,
            "yolo" | "full" => Self::Yolo,
            "guarded" => Self::Guarded,
            // "build", "confirm", "" and anything else: default to asking.
            _ => Self::Build,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Edit => "edit",
            Self::Yolo => "yolo",
            Self::Guarded => "guarded",
        }
    }
}

impl fmt::Display for PermMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Permission-gate verdict for one tool invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Execute without asking.
    Allow,
    /// Emit a `PermissionRequired` event and wait for the user decision.
    Ask,
    /// Refuse without asking (plan mode on write-class tools).
    Deny,
}

/// Tools that never trigger the gate (read-only inspection).
pub fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "Read" | "Glob" | "Grep" | "TodoRead" | "CronList" | "EnterPlanMode" | "ExitPlanMode"
    )
}

/// File-editing tools: auto-approved by `edit`, asked by `build`.
pub fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "Write" | "Edit")
}

/// The explicitly recognized dangerous shell command fragments for
/// `guarded` mode (ZCode OffPeakCreate description wording).
const DANGEROUS_COMMAND_FRAGMENTS: &[&str] = [
    "rm -rf",
    "rmdir /s",
    "del /f /s /q",
    "git reset --hard",
    "git push --force",
    "git push -f",
    "git clean -fd",
    "git checkout -- .",
    "git restore .",
    "rsync --delete",
    "format ",
    "diskpart",
    "shutdown",
    "taskkill /f",
    // Lowercase: matched against the lowercased command text.
    "remove-item -recurse -force",
    "rd /s /q",
]
.as_slice();

/// Returns true when the command matches one of the guarded-mode danger
/// fragments.
pub fn is_dangerous_command(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    DANGEROUS_COMMAND_FRAGMENTS
        .iter()
        .any(|fragment| lowered.contains(fragment))
}

/// Applies the gate: tool name + mode (+ command for `Bash`) → verdict.
pub fn gate(name: &str, mode: PermMode, command: Option<&str>) -> Verdict {
    if is_read_only_tool(name) {
        return Verdict::Allow;
    }
    match mode {
        PermMode::Plan => Verdict::Deny,
        PermMode::Yolo => {
            // guarded-only pause does not apply to plain yolo.
            Verdict::Allow
        }
        PermMode::Guarded => {
            if name == "Bash" && command.is_some_and(is_dangerous_command) {
                Verdict::Ask
            } else {
                Verdict::Allow
            }
        }
        PermMode::Edit => {
            if is_file_edit_tool(name) {
                Verdict::Allow
            } else {
                Verdict::Ask
            }
        }
        PermMode::Build => Verdict::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_legacy_aliases_and_defaults_to_build() {
        assert_eq!(PermMode::parse(Some("plan")), PermMode::Plan);
        assert_eq!(PermMode::parse(Some("build")), PermMode::Build);
        assert_eq!(PermMode::parse(Some("edit")), PermMode::Edit);
        assert_eq!(PermMode::parse(Some("autoEdit")), PermMode::Edit);
        assert_eq!(PermMode::parse(Some("yolo")), PermMode::Yolo);
        assert_eq!(PermMode::parse(Some("full")), PermMode::Yolo);
        assert_eq!(PermMode::parse(Some("guarded")), PermMode::Guarded);
        assert_eq!(PermMode::parse(Some("confirm")), PermMode::Build);
        assert_eq!(PermMode::parse(Some("")), PermMode::Build);
        assert_eq!(PermMode::parse(None), PermMode::Build);
        assert_eq!(PermMode::parse(Some("bogus")), PermMode::Build);
    }

    #[test]
    fn gate_matrix_over_five_modes() {
        // Read-only tools run everywhere, including plan.
        assert_eq!(gate("Read", PermMode::Plan, None), Verdict::Allow);
        assert_eq!(gate("Grep", PermMode::Guarded, None), Verdict::Allow);
        // Plan denies everything else outright.
        assert_eq!(gate("Write", PermMode::Plan, None), Verdict::Deny);
        assert_eq!(gate("Bash", PermMode::Plan, Some("dir")), Verdict::Deny);
        // Build asks for all write-class tools.
        assert_eq!(gate("Write", PermMode::Build, None), Verdict::Ask);
        assert_eq!(gate("Bash", PermMode::Build, Some("dir")), Verdict::Ask);
        // Edit auto-approves file edits, still asks for shell.
        assert_eq!(gate("Write", PermMode::Edit, None), Verdict::Allow);
        assert_eq!(gate("Edit", PermMode::Edit, None), Verdict::Allow);
        assert_eq!(gate("Bash", PermMode::Edit, Some("dir")), Verdict::Ask);
        // Yolo runs everything.
        assert_eq!(gate("Bash", PermMode::Yolo, Some("rm -rf x")), Verdict::Allow);
        // Guarded runs everything except recognized dangerous commands.
        assert_eq!(gate("Bash", PermMode::Guarded, Some("dir")), Verdict::Allow);
        assert_eq!(gate("Bash", PermMode::Guarded, Some("git push --force")), Verdict::Ask);
        assert_eq!(gate("Write", PermMode::Guarded, None), Verdict::Allow);
    }

    #[test]
    fn dangerous_command_fragments_recognized_case_insensitively() {
        assert!(is_dangerous_command("RM -RF build"));
        assert!(is_dangerous_command("git reset --hard HEAD~1"));
        assert!(is_dangerous_command("Remove-Item -Recurse -Force ."));
        assert!(!is_dangerous_command("cargo build"));
        assert!(!is_dangerous_command("git status"));
    }
}
