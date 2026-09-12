//! H1 感知快照（design.md §22.4）。
//!
//! UIA 树/控件语义采集：按钮文字(Name)、句柄(HWND)、ControlType、
//! AutomationId、ClassName、窗口/进程、状态、坐标、label 关联。
//!
//! - [`ControlProbe`]：只读感知统一接口（生产 = [`uia::UiaProbe`]，经
//!   [`platform_probe`] 获取；测试/协议回环 = [`synth::SyntheticProbe`]）；
//! - [`capture_scene`]：异常期 [`SceneSnapshot`] 实采组装（前台窗口 + 目标
//!   控件 + 裁剪树 + 同进程弹窗；步骤 36 接线，serve `capture_scene` op）；
//! - [`screen`]：只读屏幕截图原语 `capture_screen`（v1.13——GDI BitBlt 抓
//!   整虚拟屏 + WIC 内存编码 PNG，不落盘；经 tools 感知工具面下发
//!   `pngBase64/width/height`，design.md §10.2「每步开始前截图」的
//!   harness 落点）；
//! - [`synth`]：合成控件树 + 命中/焦点/label 关联的**纯逻辑**实现——
//!   harness 全链路（graph/tools/serve/store）单测的底座，亦供 app 侧
//!   注入 mock；真实 UIA 冒烟走 `#[ignore]`（见 `tests/uia_smoke.rs`）。
//!
//! label 关联（§22.4「label 关联」）：启发式——控件的**前一个 Text 兄弟**
//! 即其标签（Windows 表单/对话框的常见布局）；产物为
//! [`ControlObservation::label`]（标签控件自身的 ControlRef），由
//! [`graph::ControlGraphBuilder::ingest_observation`] 转成 `label-for` 边。
//!
//! 密码控件遮蔽（§22.7 红线落点）：[`PASSWORD_PLACEHOLDER`] + 
//! [`mask_password_text`]（纯逻辑单测锁定）；生产 UIA（[`uia`]）在
//! control_at/focused/dump_tree/value_at 各采集点对 `IsPassword` 控件
//! 统一套用，真实值不出感知层。

use std::sync::Arc;

use crate::harness::store::{redact_scene, ControlRef, ControlRect, Redactor, SceneNode, SceneSnapshot};

/// Linux 平台接线层（upgrade.md §7.4：backends 探测 / AT-SPI probe /
/// X11·portal 截屏入口；feature 门控见其模块文档）。
#[cfg(target_os = "linux")]
pub mod linux;
pub mod screen;
pub mod synth;
#[cfg(windows)]
pub mod uia;

/// 窗口引用（list_windows / foreground_window 的行）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowRef {
    /// 顶层窗口句柄（HWND；i64 承载）。
    pub handle: i64,
    pub title: String,
    /// 进程名（如 "notepad.exe"；未知为空串）。
    pub process_name: String,
}

/// 一次控件观测：目标控件 + 可选的 label 关联（label 控件引用）。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlObservation {
    pub control: ControlRef,
    /// 启发式命中的标签控件（前一个 Text 兄弟）；None = 未命中。
    pub label: Option<ControlRef>,
}

/// 只读感知接口（§22.7：工具面默认只读；操作类不在此层）。
///
/// 实现须知：可能阻塞（COM/UIA 调用无强制超时），调用方必须放入阻塞线程
/// 并外加超时（app 侧 `spawn_blocking` + tokio timeout，与元素解析同规矩）。
pub trait ControlProbe: Send + Sync {
    /// 屏幕坐标处的控件（点击元素解析）；None = 无元素/拒绝/不可用。
    fn control_at(&self, x: f64, y: f64) -> Option<ControlObservation>;
    /// 当前键盘焦点控件（键入段首解析/密码红线辅助）。
    fn focused(&self) -> Option<ControlObservation>;
    /// 前台顶层窗口。
    fn foreground_window(&self) -> Option<WindowRef>;
    /// 可见顶层窗口清单（按 z 序，先前台）。
    fn list_windows(&self) -> Vec<WindowRef>;
    /// 前台窗口控件树的裁剪转储（深度/节点数上限防巨型树）。
    fn dump_tree(&self, max_depth: u32, max_nodes: usize) -> Vec<SceneNode>;
    /// 坐标处控件值读回（ValuePattern，Name 兜底；v1.11 回放断言/调试用）。
    /// 合成探测缺省 None（生产 UIA 实现覆盖）。
    fn value_at(&self, _x: f64, _y: f64) -> Option<String> {
        None
    }
}

/// 生产探测（Windows = 真实 UIA；Linux = AT-SPI2（linux::platform_probe_linux，
/// feature `linux-atspi`）；其余平台 None。初始化失败 → None，调用方降级为
/// 无 ControlRef 回填，事件照发——与既有 UIA 查询的 best-effort 同语义）。
pub fn platform_probe() -> Option<Arc<dyn ControlProbe>> {
    #[cfg(windows)]
    {
        match uia::UiaProbe::new() {
            Ok(probe) => Some(Arc::new(probe)),
            Err(e) => {
                // P3-13：降级现场入结构化日志（warn 级、带错误链）——感知
                // 「悄悄不可用」是排障第一现场，原 eprintln 不可聚合；lib
                // 侧只发事件，无 subscriber 时为 no-op（bin 侧装到 stderr）。
                tracing::warn!(error = %e, "UIA probe 初始化失败，感知降级不可用");
                None
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        linux::platform_probe_linux()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/* --- 密码控件遮蔽（§22.7 红线 harness 侧落点） -------------------------------- */

/// 密码控件自由文本的固定占位符：`IsPassword=true` 的控件，其 Name /
/// 读值 / 树转储 Name 一律以它承载，真实值不出感知层（§22.7「密码框/
/// 密钥跳过」在 harness 侧的落点；宿主 Redactor 之外的感知层兜底）。
pub const PASSWORD_PLACEHOLDER: &str = "<password>";

/// 密码遮蔽（纯逻辑，单测锁定）：`is_password=true` → 自由文本替换为
/// [`PASSWORD_PLACEHOLDER`]，否则原样返回。只作用于自由文本（Name/读值），
/// 结构性字段（handle/AutomationId/ClassName/rect）不遮蔽——保留定位锚点，
/// 回放/自愈仍可达该控件。
pub fn mask_password_text(is_password: bool, text: impl Into<String>) -> String {
    if is_password {
        PASSWORD_PLACEHOLDER.to_string()
    } else {
        text.into()
    }
}

/* --- 异常期 Scene 采集（§22.5/§22.6，步骤 36 接线） --------------------------- */

/// Scene 采集参数（serve `capture_scene` op 的 args 形态；缺省值与
/// [`crate::harness::tools`] 的 dump_tree 上限同源）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneCaptureOptions {
    /// 失败/目标控件屏幕坐标（Some → `control_at` 定位；None → 焦点控件兜底；
    /// 坐标给出但未命中 → targetControl 缺席，不回落焦点）。
    pub target: Option<(f64, f64)>,
    /// 控件树深度上限（防巨型树）。
    pub max_depth: u32,
    /// 控件树节点数上限。
    pub max_nodes: usize,
}

impl Default for SceneCaptureOptions {
    fn default() -> Self {
        Self {
            target: None,
            max_depth: crate::harness::tools::DUMP_TREE_DEFAULT_DEPTH,
            max_nodes: crate::harness::tools::DUMP_TREE_MAX_NODES_DEFAULT,
        }
    }
}

/// 顶层窗口引用 → 窗口级 ControlRef（§22.5「窗口级 ControlRef」；未知字段
/// 按契约承载：文本空串、句柄 0、rect 全 0——harness 不重复查询窗口矩形，
/// 坐标锚点由 treePruned 的树路径承载）。
pub fn window_control_ref(window: &WindowRef) -> ControlRef {
    ControlRef {
        handle: window.handle,
        control_type: "Window".into(),
        name: window.title.clone(),
        automation_id: String::new(),
        class_name: String::new(),
        window_title: window.title.clone(),
        process_name: window.process_name.clone(),
        rect: ControlRect::default(),
        enabled: None,
        path: None,
    }
}

/// 一次 SceneSnapshot 实采组装（§22.6 异常期 Sense；**只读**——仅调用
/// [`ControlProbe`] 的感知原语，不点击不注入，§22.7 红线）：
///
/// - `window` = 前台顶层窗口（窗口级 ControlRef）；
/// - `target_control` = 失败/目标控件（坐标命中优先，缺省取键盘焦点）；
/// - `tree_pruned` = 前台窗口控件树（深度/节点数双上限）；
/// - `dialogs` = 与前台**同进程**的其它可见顶层窗口（弹窗归属的
///   只读近似；跨进程窗口与空进程名一律不计入）；
/// - `screenshots` = 空数组（截图由宿主决定，harness 只登记路径，§22.5）；
/// - `redact` = 宿主注入的脱敏钩子（None = 恒等，exe 子进程形态缺省）。
///
/// 无前台窗口（会话无交互桌面）→ None（调用方按「采集失败」降级）。
pub fn capture_scene(
    probe: &dyn ControlProbe,
    opts: SceneCaptureOptions,
    redact: Option<&Redactor>,
) -> Option<SceneSnapshot> {
    let foreground = probe.foreground_window()?;
    let window = window_control_ref(&foreground);
    let target_control = match opts.target {
        Some((x, y)) => probe.control_at(x, y).map(|obs| obs.control),
        None => probe.focused().map(|obs| obs.control),
    };
    let tree_pruned = probe.dump_tree(opts.max_depth, opts.max_nodes);
    let dialogs = probe
        .list_windows()
        .iter()
        .filter(|w| {
            w.handle != foreground.handle
                && !w.process_name.is_empty()
                && w.process_name == foreground.process_name
        })
        .map(window_control_ref)
        .collect();
    let scene = SceneSnapshot {
        at: now_ms(),
        window,
        target_control,
        tree_pruned,
        dialogs,
        screenshots: Vec::new(),
    };
    Some(redact_scene(&scene, redact))
}

/// 当前时刻（epoch ms）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! harness 内部测试共用：合成树样例（graph/tools/serve 测试同源）。
    use super::synth::SynthNode;
    use super::WindowRef;
    use crate::harness::store::{ControlRef, ControlRect};

    pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> ControlRect {
        ControlRect { x, y, width: w, height: h }
    }

    pub(crate) fn control(control_type: &str, name: &str, r: ControlRect) -> ControlRef {
        ControlRef {
            handle: 42,
            control_type: control_type.into(),
            name: name.into(),
            automation_id: String::new(),
            class_name: control_type.into(),
            window_title: "报销单 - 记事本".into(),
            process_name: "notepad.exe".into(),
            rect: r,
            enabled: Some(true),
            path: None,
        }
    }

    /// 窗口(0,0,800,600)
    /// └─ Pane(0,0,800,600)
    ///    ├─ Button「保存」(10,10,60,24)
    ///    ├─ Text「名称」(10,100,60,24)      ← Edit 的前一个 Text 兄弟
    ///    └─ Edit「」(10,100,300,24，focused)
    pub(crate) fn sample_tree() -> SynthNode {
        SynthNode::branch(
            control("Window", "报销单 - 记事本", rect(0.0, 0.0, 800.0, 600.0)),
            vec![SynthNode::branch(
                control("Pane", "表单", rect(0.0, 0.0, 800.0, 600.0)),
                vec![
                    SynthNode::leaf(control("Button", "保存", rect(10.0, 10.0, 60.0, 24.0))),
                    SynthNode::leaf(control("Text", "名称", rect(10.0, 100.0, 60.0, 24.0))),
                    SynthNode::focused_leaf(control("Edit", "", rect(10.0, 100.0, 300.0, 24.0))),
                ],
            )],
        )
    }

    pub(crate) fn sample_windows() -> Vec<WindowRef> {
        vec![
            WindowRef { handle: 42, title: "报销单 - 记事本".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 7, title: "工作 Recorder".into(), process_name: "workrecorder.exe".into() },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::synth::{SyntheticProbe, SynthNode};
    use super::testutil::{control, rect, sample_tree, sample_windows};
    use super::*;

    fn probe() -> SyntheticProbe {
        SyntheticProbe::new(sample_tree(), sample_windows())
    }

    #[test]
    fn control_at_hits_innermost_containing_node() {
        // Edit 命中（Text 与 Edit 起点同为 (10,100) 且都含该点——按
        // 「兄弟序靠后者胜」取 Edit，模拟 UIA 的 topmost 语义）。
        let obs = probe().control_at(20.0, 110.0).expect("Edit 命中");
        assert_eq!(obs.control.control_type, "Edit");
        assert_eq!(obs.control.rect, rect(10.0, 100.0, 300.0, 24.0));
        // Button 精确命中。
        let obs = probe().control_at(15.0, 15.0).expect("Button 命中");
        assert_eq!(obs.control.name, "保存");
    }

    #[test]
    fn control_at_miss_outside_window_is_none() {
        assert!(probe().control_at(900.0, 700.0).is_none());
    }

    #[test]
    fn control_at_attaches_previous_text_sibling_as_label() {
        let obs = probe().control_at(20.0, 110.0).expect("Edit 命中");
        let label = obs.label.expect("Edit 前一个 Text 兄弟应命中 label 关联");
        assert_eq!(label.control_type, "Text");
        assert_eq!(label.name, "名称");
        // Button 是 Pane 首子，无前邻 → None。
        let obs = probe().control_at(15.0, 15.0).expect("Button 命中");
        assert!(obs.label.is_none(), "首子无前邻兄弟");
    }

    #[test]
    fn focused_returns_flagged_node_with_label() {
        let obs = probe().focused().expect("树内有 focused 标记");
        assert_eq!(obs.control.control_type, "Edit");
        assert!(obs.label.is_some());
    }

    #[test]
    fn control_at_annotates_tree_path() {
        let obs = probe().control_at(20.0, 110.0).unwrap();
        assert_eq!(
            obs.control.path.as_deref(),
            Some("Window[报销单 - 记事本]/Pane[表单]/Edit[]"),
            "路径 = 祖先链 ControlType[name]"
        );
    }

    #[test]
    fn windows_and_foreground_follow_injected_list() {
        let p = probe();
        assert_eq!(p.foreground_window().as_ref(), sample_windows().first());
        assert_eq!(p.list_windows().len(), 2);
    }

    #[test]
    fn dump_tree_respects_depth_and_node_caps() {
        let p = probe();
        let all = p.dump_tree(8, 100);
        assert_eq!(all.len(), 5, "全树 5 节点（窗口+Pane+Button+Text+Edit）");
        assert_eq!(all[0].path, "0", "根路径");
        assert_eq!(all[3].name, "名称");
        assert_eq!(p.dump_tree(1, 100).len(), 1, "深度 1 只剩根");
        assert_eq!(p.dump_tree(8, 2).len(), 2, "节点上限截断");
    }

    #[test]
    fn empty_probe_degrades_to_none() {
        let p = SyntheticProbe::new(
            SynthNode::leaf(control("Window", "", rect(0.0, 0.0, 0.0, 0.0))),
            Vec::new(),
        );
        // 空矩形窗口不含任何点 → control_at None；窗口清单空。
        assert!(p.control_at(1.0, 1.0).is_none(), "空矩形不含点");
        assert!(p.list_windows().is_empty());
    }

    /// §22.7：密码遮蔽纯逻辑——is_password=true 时自由文本占位、结构
    /// 字段（handle/className/automationId）不受影响（锚点保留）。
    #[test]
    fn password_masking_placeholders_free_text_only() {
        assert_eq!(mask_password_text(true, "hunter2"), PASSWORD_PLACEHOLDER);
        assert_eq!(
            mask_password_text(true, ""),
            PASSWORD_PLACEHOLDER,
            "空名同样占位：保留「这是密码控件」信号"
        );
        assert_eq!(mask_password_text(false, "hunter2"), "hunter2");
        // 感知层套用方式演示：只动 name，锚点字段原样。
        let mut c = control("Edit", "hunter2", rect(0.0, 0.0, 10.0, 10.0));
        c.name = mask_password_text(true, c.name.clone());
        assert_eq!(c.name, PASSWORD_PLACEHOLDER);
        assert_eq!(c.handle, 42);
        assert_eq!(c.class_name, "Edit");
        assert_eq!(c.automation_id, "");
    }

    /* --- capture_scene（§22.5/§22.6，步骤 36） ---------------------------------- */

    use super::{capture_scene, window_control_ref, SceneCaptureOptions};
    use crate::harness::store::{ControlRect, Redactor, SceneSnapshot};
    use std::sync::Arc as StdArc;

    #[test]
    fn capture_scene_assembles_foreground_focus_tree_and_empty_dialogs() {
        let scene = capture_scene(
            &probe(),
            SceneCaptureOptions::default(),
            None,
        )
        .expect("合成树有前台窗口");
        // at：合理 epoch 毫秒（2026-09-07 前后 ±10 年）。
        assert!(
            scene.at > 1_200_000_000_000 && scene.at < 2_200_000_000_000,
            "at 应为 epoch ms: {}",
            scene.at
        );
        // window = 前台（窗口级 ControlRef 映射）。
        assert_eq!(scene.window.handle, 42);
        assert_eq!(scene.window.control_type, "Window");
        assert_eq!(scene.window.name, "报销单 - 记事本");
        assert_eq!(scene.window.window_title, "报销单 - 记事本");
        assert_eq!(scene.window.process_name, "notepad.exe");
        assert_eq!(scene.window.rect, ControlRect::default(), "窗口矩形未知按全 0");
        // targetControl 缺省 = 焦点控件（Edit）。
        let target = scene.target_control.expect("焦点控件兜底");
        assert_eq!(target.control_type, "Edit");
        // treePruned = 全树 5 节点（dump_tree 缺省上限）。
        assert_eq!(scene.tree_pruned.len(), 5);
        assert_eq!(scene.tree_pruned[0].path, "0");
        // dialogs：同进程过滤后无（第二窗口是 workrecorder.exe）。
        assert!(scene.dialogs.is_empty(), "{:?}", scene.dialogs);
        // screenshots：截图由宿主决定，恒空数组。
        assert!(scene.screenshots.is_empty());
    }

    #[test]
    fn capture_scene_target_prefers_coordinates_and_miss_yields_none() {
        let opts = SceneCaptureOptions { target: Some((15.0, 15.0)), ..Default::default() };
        let scene = capture_scene(&probe(), opts, None).unwrap();
        assert_eq!(
            scene.target_control.as_ref().expect("坐标命中").name,
            "保存",
            "坐标命中优先于焦点"
        );
        // 坐标给出但未命中 → targetControl 缺席（不回落焦点）。
        let opts = SceneCaptureOptions { target: Some((900.0, 700.0)), ..Default::default() };
        let scene = capture_scene(&probe(), opts, None).unwrap();
        assert!(scene.target_control.is_none(), "未命中不回落焦点");
    }

    #[test]
    fn capture_scene_limits_tree_by_options() {
        let opts = SceneCaptureOptions { target: None, max_depth: 1, max_nodes: 100 };
        let scene = capture_scene(&probe(), opts, None).unwrap();
        assert_eq!(scene.tree_pruned.len(), 1, "深度 1 只剩根");
        let opts = SceneCaptureOptions { target: None, max_depth: 8, max_nodes: 2 };
        let scene = capture_scene(&probe(), opts, None).unwrap();
        assert_eq!(scene.tree_pruned.len(), 2, "节点上限截断");
    }

    #[test]
    fn capture_scene_dialogs_filter_same_process_windows() {
        let windows = vec![
            WindowRef { handle: 42, title: "报销单 - 记事本".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 43, title: "确认保存".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 7, title: "工作 Recorder".into(), process_name: "workrecorder.exe".into() },
            WindowRef { handle: 8, title: String::new(), process_name: String::new() },
        ];
        let p = SyntheticProbe::new(sample_tree(), windows);
        let scene = capture_scene(&p, SceneCaptureOptions::default(), None).unwrap();
        assert_eq!(scene.dialogs.len(), 1, "仅同进程其它可见窗口计入弹窗");
        assert_eq!(scene.dialogs[0].handle, 43);
        assert_eq!(scene.dialogs[0].control_type, "Window");
        assert_eq!(scene.dialogs[0].name, "确认保存");
    }

    #[test]
    fn capture_scene_applies_redactor_to_free_text_only() {
        let redact: Redactor =
            StdArc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let tree = SynthNode::branch(
            {
                let mut c = control("Window", "主窗口 sk-live-abc", rect(0.0, 0.0, 800.0, 600.0));
                c.process_name = "notepad.exe".into();
                c
            },
            vec![SynthNode::focused_leaf(control("Edit", "内容 sk-live-abc", rect(0.0, 0.0, 100.0, 24.0)))],
        );
        // 前台窗口（列表首项）须与树根同窗口（handle 42 / 同名标题）——
        // capture_scene 的 window 取自 foreground_window()。
        let windows = vec![
            WindowRef { handle: 42, title: "主窗口 sk-live-abc".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 43, title: "确认保存".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 7, title: "工作 Recorder".into(), process_name: "workrecorder.exe".into() },
        ];
        let p = SyntheticProbe::new(tree, windows);
        let scene = capture_scene(&p, SceneCaptureOptions::default(), Some(&redact)).unwrap();
        assert_eq!(scene.window.name, "主窗口 <redacted>", "窗口标题脱敏");
        assert_eq!(scene.window.process_name, "notepad.exe", "结构性字段不脱敏");
        assert_eq!(scene.target_control.as_ref().unwrap().name, "内容 <redacted>");
        assert_eq!(scene.tree_pruned[0].name, "主窗口 <redacted>", "树节点 name 脱敏");
        assert_eq!(scene.dialogs.len(), 1, "同进程其它窗口计入弹窗（43 号）");
        // None = 恒等。
        let raw = capture_scene(&p, SceneCaptureOptions::default(), None).unwrap();
        assert_eq!(raw.window.name, "主窗口 sk-live-abc");
    }

    #[test]
    fn window_control_ref_maps_contract_fields() {
        let w = WindowRef { handle: 7, title: "t".into(), process_name: "p.exe".into() };
        let r = window_control_ref(&w);
        assert_eq!(r.control_type, "Window");
        assert_eq!(r.handle, 7);
        assert_eq!(r.name, "t");
        assert_eq!(r.automation_id, "");
        assert_eq!(r.class_name, "");
        assert_eq!(r.enabled, None);
        assert_eq!(r.path, None);
    }

    #[test]
    fn capture_scene_without_foreground_is_none() {
        let p = SyntheticProbe::new(sample_tree(), Vec::new());
        assert!(capture_scene(&p, SceneCaptureOptions::default(), None).is_none());
    }

    /// capture_scene 返回值可无损 serde 往返（scene.json 契约形态，camelCase）。
    #[test]
    fn capture_scene_output_roundtrips_scene_snapshot() {
        let scene = capture_scene(&probe(), SceneCaptureOptions::default(), None).unwrap();
        let text = serde_json::to_string(&scene).unwrap();
        assert!(text.contains("\"treePruned\""), "camelCase 契约键: {text}");
        assert!(text.contains("\"targetControl\""));
        let back: SceneSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(back, scene);
    }
}
