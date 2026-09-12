//! H5 落盘与契约（design.md §22.4/§22.5）。
//!
//! - **契约**：[`ControlRef`] / [`UiAssociation`] / [`SceneSnapshot`]——
//!   app 侧镜像为 `common/harness.ts` 的 zod schema，双侧以
//!   `fixtures/harness-contract-golden.json` golden 对齐（harness cargo test
//!   断言 serde 往返；vitest 断言 zod 解析，防双实现漂移）；
//! - **格式**：`sessions/<id>/ui/control-graph.json`（录制期关联图，§15）与
//!   `runs/<ts>/scene.json`（异常期现场快照，步骤 36 接线）；
//! - **脱敏钩子**：[`Redactor`]——宿主注入的自由文本脱敏函数（app 侧接
//!   sensitive 引擎，§22.7「密码框/密钥跳过沿用既有规则」）；harness 侧
//!   默认不注入（等价恒等），落盘时只应用于自由文本字段（name/windowTitle），
//!   不碰结构性字段（handle/controlType/rect）。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::harness::Error;

/// 关联图文档版本（`control-graph.json` 的 `version` 字段）。
pub const CONTROL_GRAPH_VERSION: u32 = 1;

/// 录制期关联图相对会话目录的落盘路径（§15）。
pub const CONTROL_GRAPH_REL: &str = "ui/control-graph.json";

/// 异常期 Scene 快照文件名（§15 `runs/<ts>/scene.json`，步骤 36 接线）。
pub const SCENE_FILE: &str = "scene.json";

/* --- 契约类型（§14/§22.5；与 app/common/harness.ts zod 双轨） ---------------- */

/// 控件矩形（屏幕物理像素；`rect` 契约字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 控件引用（design.md §14/§22.5）：句柄 + 文字锚点，供 LLM 直接定位。
///
/// 必填字段一律非 Option（缺知的文本字段以空串承载，句柄未知记 0）——
/// 保证 golden/生成物形态稳定；可选字段仅 `enabled` / `path`（契约原文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlRef {
    /// 控件原生窗口句柄（HWND；未知为 0）。
    pub handle: i64,
    /// UIA ControlType 名（如 "Button" / "Edit" / "Custom"）。
    pub control_type: String,
    /// UIA Name（按钮文字等；未知为空串）。
    pub name: String,
    /// UIA AutomationId（未知为空串）。
    pub automation_id: String,
    /// UIA ClassName（未知为空串）。
    pub class_name: String,
    /// 控件所在顶层窗口标题（未知为空串）。
    pub window_title: String,
    /// 控件所属进程名（如 "notepad.exe"；未知为空串）。
    pub process_name: String,
    /// 屏幕矩形（物理像素）。
    pub rect: ControlRect,
    /// UIA IsEnabled（未知缺席）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// 控件树路径（如 `Window[报销单]/Button[提交]`；未知缺席）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 关联边类型（§22.5）：child-of / label-for / dialog-of / focus-chain /
/// step-acted-on。serde 字面量锁定（golden 双侧断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssociationKind {
    #[serde(rename = "child-of")]
    ChildOf,
    #[serde(rename = "label-for")]
    LabelFor,
    #[serde(rename = "dialog-of")]
    DialogOf,
    #[serde(rename = "focus-chain")]
    FocusChain,
    #[serde(rename = "step-acted-on")]
    StepActedOn,
}

/// 关联边（design.md §14/§22.5）：`{ from, to, kind }`；from/to 为图内
/// 节点 id（`ctrl:*` / `win:*` / `proc:*` / `step:*`，见 [`graph`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiAssociation {
    pub from: String,
    pub to: String,
    pub kind: AssociationKind,
}

/// SceneSnapshot 的裁剪树节点（flat 列表 + `path` 编码层级，见
/// [`SceneSnapshot::tree_pruned`]）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneNode {
    /// 树路径（根起 `/` 连接的子序号，如 `0/2/5`）。
    pub path: String,
    pub control_type: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// 异常期 UI 现况快照（design.md §14/§22.5；步骤 36 接线）：
/// `{ at, window, targetControl?, treePruned[], dialogs[], screenshots[] }`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneSnapshot {
    /// 采集时刻（epoch ms）。
    pub at: i64,
    /// 前台窗口（窗口级 ControlRef）。
    pub window: ControlRef,
    /// 失败/目标控件（如可定位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_control: Option<ControlRef>,
    /// 剪裁后的前台窗口控件树（深度/节点数上限见 snapshot 探测参数）。
    #[serde(default)]
    pub tree_pruned: Vec<SceneNode>,
    /// 可见弹窗（窗口级 ControlRef 列表）。
    #[serde(default)]
    pub dialogs: Vec<ControlRef>,
    /// 截图文件相对路径（截图采集由宿主决定，harness 只登记路径）。
    #[serde(default)]
    pub screenshots: Vec<String>,
}

/* --- 关联图文档（§15 control-graph.json） ------------------------------------ */

/// 进程节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessNode {
    pub id: String,
    pub name: String,
}

/// 窗口节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowNode {
    pub id: String,
    pub handle: i64,
    pub title: String,
    /// 所属进程节点 id。
    pub process_id: String,
}

/// 控件节点：ControlRef + 图内 id + 触达它的事件 seq（按升序去重）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlNode {
    pub id: String,
    pub control: ControlRef,
    pub event_seqs: Vec<u64>,
}

/// 录制期关联图（`sessions/<id>/ui/control-graph.json`，§15/§22.5）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlGraph {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub processes: Vec<ProcessNode>,
    pub windows: Vec<WindowNode>,
    pub controls: Vec<ControlNode>,
    pub associations: Vec<UiAssociation>,
}

/* --- 脱敏钩子（§22.7） ------------------------------------------------------- */

/// 脱敏钩子：宿主注入的自由文本脱敏函数（app 侧接 sensitive 引擎；
/// None/缺省 = 恒等，不出网前 app 仍有强制敏感扫描兜底）。
pub type Redactor = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// 对 ControlRef 的自由文本字段（name / windowTitle）应用脱敏钩子；
/// 结构性字段（handle/controlType/automationId/className/processName/rect）
/// 不脱敏（非用户自由文本，脱敏无信息增益且破坏锚点）。
pub fn redact_control_ref(control: &ControlRef, redact: Option<&Redactor>) -> ControlRef {
    let Some(redact) = redact else {
        return control.clone();
    };
    let mut out = control.clone();
    out.name = redact(&control.name);
    out.window_title = redact(&control.window_title);
    out
}

/// 对 SceneSnapshot 的自由文本字段（各 ControlRef 的 name/windowTitle、
/// 树节点 name）应用脱敏钩子。
pub fn redact_scene(scene: &SceneSnapshot, redact: Option<&Redactor>) -> SceneSnapshot {
    let Some(redact) = redact else {
        return scene.clone();
    };
    let redact_node = |node: &SceneNode| SceneNode {
        name: redact(&node.name),
        ..node.clone()
    };
    SceneSnapshot {
        window: redact_control_ref(&scene.window, Some(redact)),
        target_control: scene.target_control.as_ref().map(|c| redact_control_ref(c, Some(redact))),
        tree_pruned: scene.tree_pruned.iter().map(redact_node).collect(),
        dialogs: scene.dialogs.iter().map(|c| redact_control_ref(c, Some(redact))).collect(),
        ..scene.clone()
    }
}

/* --- 落盘（原子写：tmp + rename，与 app 会话产物同惯例） ---------------------- */

/// 把关联图写入 `<session_dir>/ui/control-graph.json`（先脱敏后序列化；
/// 原子写，崩溃不落半文件）。返回落盘绝对路径。
pub fn write_control_graph(
    session_dir: &Path,
    graph: &ControlGraph,
    redact: Option<&Redactor>,
) -> Result<PathBuf, Error> {
    let doc = redact_graph(graph, redact);
    let path = session_dir.join(CONTROL_GRAPH_REL);
    write_atomic(&path, &doc)?;
    Ok(path)
}

/// 脱敏后的关联图（写盘与读回校验共用）。
pub fn redact_graph(graph: &ControlGraph, redact: Option<&Redactor>) -> ControlGraph {
    let Some(redact) = redact else {
        return graph.clone();
    };
    ControlGraph {
        windows: graph
            .windows
            .iter()
            .map(|w| WindowNode { title: redact(&w.title), ..w.clone() })
            .collect(),
        controls: graph
            .controls
            .iter()
            .map(|c| ControlNode { control: redact_control_ref(&c.control, Some(redact)), ..c.clone() })
            .collect(),
        ..graph.clone()
    }
}

/// 读取会话目录下的关联图（缺文件 → Err；损坏 → Err）。
pub fn read_control_graph(session_dir: &Path) -> Result<ControlGraph, Error> {
    let path = session_dir.join(CONTROL_GRAPH_REL);
    let text = std::fs::read_to_string(&path).map_err(|source| Error::ControlGraphRead {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&text).map_err(Error::ControlGraphCorrupt)
}

/// 把 Scene 快照写入 `<run_dir>/scene.json`（先脱敏后序列化；原子写）。
/// 步骤 36（异常期）接线；本期契约与格式先行（golden 锁定）。
pub fn write_scene(
    run_dir: &Path,
    scene: &SceneSnapshot,
    redact: Option<&Redactor>,
) -> Result<PathBuf, Error> {
    let doc = redact_scene(scene, redact);
    let path = run_dir.join(SCENE_FILE);
    write_atomic(&path, &doc)?;
    Ok(path)
}

/// 原子写 JSON 文档（pretty + 尾换行；tmp + rename）。
///
/// 并发安全（P3-11）：tmp 文件名带 pid/纳秒/进程内序号后缀——同路径多
/// 写者互不践踏；rename 前对目标目录 best-effort fsync（见
/// [`sync_dir_best_effort`]），崩溃后目录项不悬空。各阶段失败经 [`Error`]
/// 变体携带 io/json 源错误（P3-13：文案与原字符串逐字节等价）。
fn write_atomic<T: serde::Serialize>(path: &Path, doc: &T) -> Result<(), Error> {
    let mut body = serde_json::to_string_pretty(doc).map_err(Error::SerializeFailed)?;
    body.push('\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::CreateDirFailed {
            dir: parent.display().to_string(),
            source,
        })?;
    }
    let tmp = tmp_path_of(path);
    {
        let mut file = std::fs::File::create(&tmp).map_err(Error::WriteTmpFailed)?;
        file.write_all(body.as_bytes()).map_err(Error::WriteTmpFailed)?;
        file.sync_all().ok();
    }
    sync_dir_best_effort(path.parent());
    std::fs::rename(&tmp, path).map_err(Error::RenameFailed)
}

/// 目标文件的临时写路径（P3-11）：`<原名>.{pid}.{nanos}.{seq}.tmp`——
/// pid 区分进程，nanos + 进程内原子序号区分线程（SystemTime 在粗时钟下
/// 多线程可能同纳秒，序号兜底），同路径并发写各拿各的 tmp。
fn tmp_path_of(path: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "doc".into());
    name.push_str(&format!(".{}.{}.{}.tmp", std::process::id(), nanos, seq));
    path.with_file_name(name)
}

/// rename 前对目标目录 best-effort fsync（P3-11 崩溃一致性：让 tmp 的
/// 目录项先于 rename 落盘）。`File::open` 即只读打开（Linux 可 sync 目录；
/// Windows 打开目录即失败）——失败一律忽略：不影响正确性，只收窄崩溃窗口。
fn sync_dir_best_effort(dir: Option<&Path>) {
    let Some(dir) = dir else { return };
    if let Ok(handle) = std::fs::File::open(dir) {
        handle.sync_all().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn sample_control() -> ControlRef {
        ControlRef {
            handle: 197_296,
            control_type: "Button".into(),
            name: "提交".into(),
            automation_id: "SubmitBtn".into(),
            class_name: "Button".into(),
            window_title: "报销单.xlsx - Excel".into(),
            process_name: "excel.exe".into(),
            rect: ControlRect { x: 480.0, y: 360.0, width: 88.0, height: 28.0 },
            enabled: Some(true),
            path: Some("Window[报销单]/Button[提交]".into()),
        }
    }

    pub(crate) fn minimal_control() -> ControlRef {
        ControlRef {
            handle: 0,
            control_type: "Edit".into(),
            name: String::new(),
            automation_id: String::new(),
            class_name: String::new(),
            window_title: String::new(),
            process_name: String::new(),
            rect: ControlRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            enabled: None,
            path: None,
        }
    }

    #[test]
    fn control_ref_serializes_camel_case_and_skips_optional() {
        let value = serde_json::to_value(sample_control()).unwrap();
        assert_eq!(
            value,
            json!({
                "handle": 197296,
                "controlType": "Button",
                "name": "提交",
                "automationId": "SubmitBtn",
                "className": "Button",
                "windowTitle": "报销单.xlsx - Excel",
                "processName": "excel.exe",
                "rect": { "x": 480.0, "y": 360.0, "width": 88.0, "height": 28.0 },
                "enabled": true,
                "path": "Window[报销单]/Button[提交]",
            })
        );
        // 最小形态：可选字段缺席、必填字段可空串/0。
        let minimal = serde_json::to_value(minimal_control()).unwrap();
        let obj = minimal.as_object().unwrap();
        assert!(!obj.contains_key("enabled") && !obj.contains_key("path"));
        assert_eq!(serde_json::from_value::<ControlRef>(minimal).unwrap(), minimal_control());
    }

    #[test]
    fn association_kinds_serde_literals_are_locked() {
        let cases = [
            (AssociationKind::ChildOf, "child-of"),
            (AssociationKind::LabelFor, "label-for"),
            (AssociationKind::DialogOf, "dialog-of"),
            (AssociationKind::FocusChain, "focus-chain"),
            (AssociationKind::StepActedOn, "step-acted-on"),
        ];
        for (kind, text) in cases {
            let value = serde_json::to_value(kind).unwrap();
            assert_eq!(value, json!(text));
            assert_eq!(serde_json::from_value::<AssociationKind>(value).unwrap(), kind);
        }
    }

    #[test]
    fn redactor_touches_only_free_text_fields() {
        let redact: Redactor = Arc::new(|s: &str| {
            if s.contains("sk-live") { "<redacted>".into() } else { s.to_string() }
        });
        let mut control = sample_control();
        control.name = "sk-live-abcdef".into();
        control.automation_id = "sk-live-should-stay".into();
        let out = redact_control_ref(&control, Some(&redact));
        assert_eq!(out.name, "<redacted>");
        assert_eq!(out.automation_id, "sk-live-should-stay", "结构性字段不脱敏");
        assert_eq!(out.window_title, control.window_title);
        // None = 恒等。
        assert_eq!(redact_control_ref(&control, None), control);
    }

    #[test]
    fn control_graph_atomic_write_read_roundtrip_with_redaction() {
        let dir = std::env::temp_dir().join(format!("harness-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let graph = ControlGraph {
            version: CONTROL_GRAPH_VERSION,
            session_id: Some("20260907-000000-deadbeef".into()),
            processes: vec![ProcessNode { id: "proc:excel.exe".into(), name: "excel.exe".into() }],
            windows: vec![WindowNode {
                id: "win:197296".into(),
                handle: 197_296,
                title: "报销单 sk-live-abcdef".into(),
                process_id: "proc:excel.exe".into(),
            }],
            controls: vec![ControlNode {
                id: "ctrl:abc".into(),
                control: sample_control(),
                event_seqs: vec![3, 7],
            }],
            associations: vec![UiAssociation {
                from: "step:step-1".into(),
                to: "ctrl:abc".into(),
                kind: AssociationKind::StepActedOn,
            }],
        };
        let redact: Redactor =
            Arc::new(|s: &str| s.replace("sk-live-abcdef", "<redacted>"));
        let path = write_control_graph(&dir, &graph, Some(&redact)).unwrap();
        assert!(path.is_file());
        // 落盘文本含脱敏结果、camelCase 字段与 kebab-case 关联边字面量。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-live-abcdef"));
        assert!(text.contains("\"controlType\""));
        assert!(text.contains("\"step-acted-on\""));
        // 读回 == 脱敏后的文档（而非原文档）。
        let read_back = read_control_graph(&dir).unwrap();
        assert_eq!(read_back, redact_graph(&graph, Some(&redact)));
        assert_eq!(read_back.controls[0].control.name, "提交", "未命中脱敏的文本原样");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3-11 回归：8 线程同路径并发 write_control_graph——tmp 名带
    /// pid/nanos/序号互不践踏，全部成功；终态为其中一份完整文档（无截断
    /// /损坏），目录无 .tmp 残留（全部被 rename 走）。
    #[test]
    fn concurrent_writes_to_same_path_stay_atomic_and_complete() {
        let dir = std::env::temp_dir().join(format!("harness-store-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let graphs: Vec<ControlGraph> = (0..8)
            .map(|i| ControlGraph {
                version: CONTROL_GRAPH_VERSION,
                session_id: Some(format!("session-{i}")),
                processes: vec![ProcessNode { id: "proc:notepad.exe".into(), name: "notepad.exe".into() }],
                windows: vec![WindowNode {
                    id: "win:42".into(),
                    handle: 42,
                    title: format!("并发窗口 {i}"),
                    process_id: "proc:notepad.exe".into(),
                }],
                controls: vec![ControlNode {
                    id: "ctrl:x".into(),
                    control: sample_control(),
                    event_seqs: vec![i],
                }],
                associations: vec![],
            })
            .collect();
        // Barrier 对齐起跑：加大同路径碰撞窗口（旧实现的固定 *.tmp 名恰在此互踩）。
        let barrier = Arc::new(std::sync::Barrier::new(graphs.len()));
        std::thread::scope(|scope| {
            for graph in &graphs {
                let barrier = barrier.clone();
                let dir = &dir;
                scope.spawn(move || {
                    barrier.wait();
                    write_control_graph(dir, graph, None).unwrap();
                });
            }
        });
        // 终态 = 八份之一完整读回（serde 解析成功本身即排除截断/交错损坏）。
        let read_back = read_control_graph(&dir).unwrap();
        assert!(
            graphs.contains(&read_back),
            "终态应为某一位并发写者的完整文档: {:?}",
            read_back.session_id
        );
        // tmp 全部被 rename 走，目录无残留。
        let leftovers: Vec<String> = std::fs::read_dir(dir.join("ui"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "并发写后无 tmp 残留: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scene_write_applies_redaction_to_all_free_text() {
        let dir = std::env::temp_dir().join(format!("harness-scene-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut window = sample_control();
        window.control_type = "Window".into();
        window.name = "主窗口 sk-live-abc".into();
        let scene = SceneSnapshot {
            at: 1_786_000_000_000,
            window: window.clone(),
            target_control: Some(minimal_control()),
            tree_pruned: vec![SceneNode {
                path: "0".into(),
                control_type: "Pane".into(),
                name: "sk-live-abc".into(),
                automation_id: None,
                class_name: Some("Pane".into()),
                enabled: Some(true),
            }],
            dialogs: vec![window],
            screenshots: vec![],
        };
        let redact: Redactor = Arc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let path = write_scene(&dir, &scene, Some(&redact)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-live-abc"));
        let read_back: SceneSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(read_back.window.name, "主窗口 <redacted>");
        assert_eq!(read_back.tree_pruned[0].name, "<redacted>");
        assert_eq!(read_back.target_control.as_ref().unwrap().control_type, "Edit");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
