//! H2 关联图（design.md §22.4）。
//!
//! 录制期把 事件↔步骤↔控件↔窗口↔进程 连成图，产出
//! `sessions/<id>/ui/control-graph.json`（§15/§22.5）：
//!
//! - 节点 id：`proc:<name>` / `win:<handle>` / `ctrl:<指纹hex>` / `step:<stepId>`
//!   （指纹 = handle+controlType+name+automationId+className+path 的稳定
//!   哈希，跨运行确定性；path 是无句柄元素的第二定位维度，见
//!   [`control_fingerprint`]）；`win:` 以**顶层窗口**为粒度——观测只带
//!   控件自身 HWND，归并仅凭「(windowTitle, processName) 下句柄已见」：
//!   同标题同进程的新句柄视为另一个窗口，不并入首见者（宁可稀释也不
//!   误并；标题未知时不并，见 [`ControlGraphBuilder::ingest`]）；
//! - 边（[`crate::harness::store::UiAssociation`]）：
//!   - `control --child-of--> window`、`window --child-of--> process`
//!     （[`ControlGraphBuilder::ingest`] 自动产出）；
//!   - `step:<id> --step-acted-on--> control`（同上，带步骤 id 时）；
//!   - `label-for` / `dialog-of` / `focus-chain`：[`ControlGraphBuilder::associate`]
//!     公开登记（label-for 由 [`ControlGraphBuilder::ingest_observation`] 从
//!     探测的 label 关联产出，label 控件一并入图、边不悬空；
//!     dialog-of/focus-chain 留给异常期/宿主扩展）；
//! - 异常期「失败控件在现况树中的归属与路径」：[`SceneOwnership`] 接口 +
//!   [`FingerprintOwnership`] 最小实现（M8b 步骤 36 接线）。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::harness::snapshot::ControlObservation;
use crate::harness::store::{
    AssociationKind, ControlGraph, ControlNode, ControlRef, ProcessNode, UiAssociation, WindowNode,
    CONTROL_GRAPH_VERSION,
};

/* --- 节点 id 与指纹（确定性；golden/单测锁定） ------------------------------ */

/// 控件指纹：句柄 + 四个文字锚点字段 + 树路径的稳定哈希（同控件重复
/// 观测归并为同节点）。path 参与哈希给无句柄元素（网页内容恒
/// handle=0）补第二定位维度：同款「删除」按钮在不同容器/窗口不再
/// 碰撞去重（P2-4）；path 缺席（None）时退回锚点语义。
pub fn control_fingerprint(control: &ControlRef) -> u64 {
    let mut hasher = DefaultHasher::new();
    control.handle.hash(&mut hasher);
    control.control_type.hash(&mut hasher);
    control.name.hash(&mut hasher);
    control.automation_id.hash(&mut hasher);
    control.class_name.hash(&mut hasher);
    control.path.hash(&mut hasher);
    hasher.finish()
}

/// 控件节点 id。
pub fn control_id(control: &ControlRef) -> String {
    format!("ctrl:{:016x}", control_fingerprint(control))
}

/// 窗口节点 id。
pub fn window_id(handle: i64) -> String {
    format!("win:{handle}")
}

/// 进程节点 id。
pub fn process_id(name: &str) -> String {
    format!("proc:{name}")
}

/// 步骤节点 id。
pub fn step_node_id(step_id: &str) -> String {
    format!("step:{step_id}")
}

/* --- 构建器 ----------------------------------------------------------------- */

/// 录制期关联图构建器（确定性；输入事件顺序，输出节点/边按首次出现序）。
#[derive(Debug, Default)]
pub struct ControlGraphBuilder {
    session_id: Option<String>,
    processes: Vec<ProcessNode>,
    process_seen: HashSet<String>,
    windows: Vec<WindowNode>,
    window_seen: HashSet<i64>,
    /// (windowTitle, processName) → 该键下已见控件句柄集合。观测只带
    /// 控件自身 HWND（无 owning window 句柄），归并的可靠信号只有
    /// 「句柄已见」；同键新句柄一律新开 win 节点（P2-3：同标题同进程
    /// 的不同窗口不误并，宁可稀释），见 [`ControlGraphBuilder::ingest`]。
    /// 将来接线 owning window 探测后，此表可升级为
    /// 「控件句柄 → 顶层窗口句柄」映射以恢复跨句柄归并。
    window_alias: HashMap<(String, String), HashSet<i64>>,
    controls: Vec<ControlNode>,
    control_seen: HashMap<String, usize>,
    associations: Vec<UiAssociation>,
    association_seen: HashSet<(String, String, AssociationKind)>,
}

impl ControlGraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 会话 id（落盘文档字段；可选）。
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// 登记一条事件触达的控件观测（探测产物，可能带 label 关联）。
    pub fn ingest_observation(
        &mut self,
        seq: u64,
        step_id: Option<&str>,
        observation: &ControlObservation,
    ) {
        self.ingest(seq, step_id, &observation.control);
        // P2-2：label 控件一并入图（不带步骤——标签不是被操作对象），
        // label-for 边两端都有 ControlNode，不再悬空；重复观测经
        // ingest 自然去重。
        if let Some(label) = &observation.label {
            self.ingest(seq, None, label);
            self.associate(
                control_id(label),
                control_id(&observation.control),
                AssociationKind::LabelFor,
            );
        }
    }

    /// 登记一条事件触达的控件：补控件/窗口/进程节点与 child-of /
    /// step-acted-on 边（去重；句柄未知(0)时跳过窗口级关联）。
    pub fn ingest(&mut self, seq: u64, step_id: Option<&str>, control: &ControlRef) {
        let control_node_id = control_id(control);
        match self.control_seen.get(&control_node_id) {
            Some(&index) => {
                let node = &mut self.controls[index];
                if !node.event_seqs.contains(&seq) {
                    node.event_seqs.push(seq);
                    node.event_seqs.sort_unstable();
                }
            }
            None => {
                self.control_seen.insert(control_node_id.clone(), self.controls.len());
                self.controls.push(ControlNode {
                    id: control_node_id.clone(),
                    control: control.clone(),
                    event_seqs: vec![seq],
                });
            }
        }

        // 窗口级关联（句柄未知 = 0 → 跳过，避免污染图）。
        if control.handle != 0 {
            // win: 节点以顶层窗口为粒度，但观测只携带控件自身 HWND：
            // 可靠的归并信号只有「句柄已见」（同窗口/同控件的重复
            // 观测共用一个节点）。P2-3：同 (windowTitle, processName)
            // 的新句柄视为另一个同标题窗口（桌面常态：两个「无标题 -
            // 记事本」），新开节点不并入首见者——宁可稀释也不误并；
            // 归并目标即控件自身句柄，window_seen 落账即可。标题未知
            // （空串）无法判同源，退回逐句柄建节点（保守）。
            let window_handle = if control.window_title.is_empty() {
                control.handle
            } else {
                self.window_alias
                    .entry((control.window_title.clone(), control.process_name.clone()))
                    .or_default()
                    .insert(control.handle);
                control.handle
            };
            let window = window_id(window_handle);
            if self.window_seen.insert(window_handle) {
                self.windows.push(WindowNode {
                    id: window.clone(),
                    handle: window_handle,
                    title: control.window_title.clone(),
                    process_id: process_id(&control.process_name),
                });
            }
            self.associate(control_node_id.clone(), window.clone(), AssociationKind::ChildOf);
            if !control.process_name.is_empty() {
                let process = process_id(&control.process_name);
                if self.process_seen.insert(process.clone()) {
                    self.processes
                        .push(ProcessNode { id: process.clone(), name: control.process_name.clone() });
                }
                self.associate(window, process, AssociationKind::ChildOf);
            }
        }

        if let Some(step_id) = step_id {
            self.associate(
                step_node_id(step_id),
                control_node_id,
                AssociationKind::StepActedOn,
            );
        }
    }

    /// 公开关联登记（label-for / dialog-of / focus-chain / 自定义扩展）；
    /// 同 (from, to, kind) 去重。from/to 为图内节点 id 字符串。
    ///
    /// debug 断言（P2-2/P3-13）：控件端点（`ctrl:` 前缀）必须已入图，
    /// 悬空边在登记点即暴露——associate 是所有边的唯一入口（ingest
    /// 亦经它登记），入口断言即全图校验，且定位比 build() 收口处更准；
    /// `win:`/`proc:`/`step:` 端点允许前向引用（dialog-of 可登记图外
    /// 窗口、step 无节点表，公开扩展面的既有语义）。自环（from == to）
    /// 同样拦截（P3-13 一跳环检测）：五种边均为跨端点语义，一跳环必是
    /// 登记方 bug；只查一跳，完整多跳环检测不在本层（多跳环可能是宿主
    /// 自定义边的合法语义）。release 行为不变（debug_assert 语义）。
    pub fn associate(&mut self, from: String, to: String, kind: AssociationKind) {
        debug_assert!(
            !from.starts_with("ctrl:") || self.control_seen.contains_key(&from),
            "悬空边：from {from} 不在控件节点表"
        );
        debug_assert!(
            !to.starts_with("ctrl:") || self.control_seen.contains_key(&to),
            "悬空边：to {to} 不在控件节点表"
        );
        debug_assert_ne!(from, to, "一跳环（自环）：{kind:?} 边两端同为 {from}");
        if self.association_seen.insert((from.clone(), to.clone(), kind)) {
            self.associations.push(UiAssociation { from, to, kind });
        }
    }

    /// 图是否为空（无任何控件证据）——pipeline 据此跳过落盘。
    pub fn is_empty(&self) -> bool {
        self.controls.is_empty()
    }

    /// 收口产出（消费构建器）。
    pub fn build(self) -> ControlGraph {
        ControlGraph {
            version: CONTROL_GRAPH_VERSION,
            session_id: self.session_id,
            processes: self.processes,
            windows: self.windows,
            controls: self.controls,
            associations: self.associations,
        }
    }
}

/* --- 异常期归属计算（接口预留 + 最小实现；M8b 步骤 36 接线） ------------------ */

/// 失败控件在现况 SceneSnapshot 树中的归属计算（§22.4 H2 异常期职责；
/// M8a 预留接口，M8b 由自愈现场流程消费）。
pub trait SceneOwnership {
    /// 失败控件在现况树中的归属路径（SceneNode.path）；None = 未定位到。
    fn locate(&self, scene: &crate::harness::store::SceneSnapshot, target: &ControlRef) -> Option<String>;
}

/// 指纹归属：target 的文字锚点四元组与树节点逐字段相等（同 [`control_fingerprint`]
/// 语义的树侧变体）。
#[derive(Debug, Clone, Copy, Default)]
pub struct FingerprintOwnership;

impl SceneOwnership for FingerprintOwnership {
    fn locate(
        &self,
        scene: &crate::harness::store::SceneSnapshot,
        target: &ControlRef,
    ) -> Option<String> {
        // 精确：名称非空 + 四锚点逐字段相等（树节点缺席字段按空串比）。
        scene
            .tree_pruned
            .iter()
            .find(|node| {
                !target.name.is_empty()
                    && node.control_type == target.control_type
                    && node.name == target.name
                    && node.automation_id.as_deref().unwrap_or("") == target.automation_id
                    && node.class_name.as_deref().unwrap_or("") == target.class_name
            })
            .map(|node| node.path.clone())
            .or_else(|| {
                // 兜底：仅 AutomationId 精确命中（按钮文字可能随语言/状态漂移）。
                scene.tree_pruned.iter().find_map(|node| {
                    (!target.automation_id.is_empty()
                        && node.automation_id.as_deref() == Some(target.automation_id.as_str()))
                        .then(|| node.path.clone())
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::snapshot::testutil::{control, rect, sample_windows};
    use crate::harness::snapshot::synth::{SyntheticProbe, SynthNode};
    use crate::harness::snapshot::ControlProbe;
    use crate::harness::store::{SceneNode, SceneSnapshot};

    fn click_control(name: &str) -> ControlRef {
        control("Button", name, rect(480.0, 360.0, 88.0, 28.0))
    }

    #[test]
    fn node_ids_are_deterministic_and_prefixed() {
        let a = click_control("提交");
        let b = click_control("提交");
        assert_eq!(control_id(&a), control_id(&b), "同指纹同 id");
        assert_ne!(control_id(&a), control_id(&click_control("取消")));
        assert!(control_id(&a).starts_with("ctrl:"));
        assert_eq!(window_id(42), "win:42");
        assert_eq!(process_id("notepad.exe"), "proc:notepad.exe");
        assert_eq!(step_node_id("step-1"), "step:step-1");
        // 空串/缺省字段变化影响指纹（锚点参与哈希）。
        let mut other = a.clone();
        other.automation_id = "X".into();
        assert_ne!(control_id(&a), control_id(&other));
    }

    #[test]
    fn ingest_builds_deduped_nodes_and_edges() {
        let mut b = ControlGraphBuilder::new().session_id("s1");
        let c1 = click_control("提交");
        let c2 = click_control("取消");
        b.ingest(3, Some("step-1"), &c1);
        b.ingest(7, Some("step-2"), &c1); // 同控件二次触达：只并 seq。
        b.ingest(9, Some("step-2"), &c2);
        let graph = b.build();
        assert_eq!(graph.version, CONTROL_GRAPH_VERSION);
        assert_eq!(graph.session_id.as_deref(), Some("s1"));
        assert_eq!(graph.controls.len(), 2);
        assert_eq!(graph.controls[0].event_seqs, vec![3, 7]);
        assert_eq!(graph.windows.len(), 1, "同句柄归并");
        assert_eq!(graph.processes.len(), 1);
        // 边：2×child-of(控件→窗口，两控件各一) + 窗口→进程 + 2×step-acted-on = 6。
        assert_eq!(graph.associations.len(), 6, "{:?}", graph.associations);
        let c1_id = control_id(&c1);
        let c2_id = control_id(&c2);
        assert_eq!(graph.controls[0].id, c1_id);
        assert!(graph.associations.iter().any(|e| e.kind == AssociationKind::StepActedOn
            && e.from == "step:step-1"
            && e.to == c1_id));
        assert!(graph.associations.iter().any(|e| e.kind == AssociationKind::StepActedOn
            && e.from == "step:step-2"
            && e.to == c2_id));
        // 同控件二次触达（seq 7 归 step-2）也产边。
        assert!(graph.associations.iter().any(|e| e.kind == AssociationKind::StepActedOn
            && e.from == "step:step-2"
            && e.to == c1_id));
    }

    #[test]
    fn ingest_skips_window_links_for_unknown_handle() {
        let mut b = ControlGraphBuilder::new();
        let mut c = click_control("提交");
        c.handle = 0;
        c.process_name = String::new();
        b.ingest(1, None, &c);
        let graph = b.build();
        assert_eq!(graph.controls.len(), 1);
        assert!(graph.windows.is_empty() && graph.processes.is_empty());
        assert!(graph.associations.is_empty(), "无句柄无边: {:?}", graph.associations);
    }

    /// 同一窗口的控件归并到一个 win 节点——P2-3 后判据是「句柄已见」：
    /// 单 HWND 框架（WPF/WinForms 窗口内控件 HWND 与顶层窗口相同）与
    /// 同控件重复观测共用一个节点；不同句柄不再凭 (title, process)
    /// 并入首见者（防误并见下一条测试）。
    #[test]
    fn ingest_merges_same_handle_controls_into_one_win_node() {
        let mut b = ControlGraphBuilder::new();
        let mut toolbar = click_control("工具栏按钮");
        toolbar.handle = 100; // 单 HWND 框架：窗口内控件 HWND 相同。
        let mut edit = click_control("输入框");
        edit.handle = 100;
        b.ingest(1, None, &toolbar);
        b.ingest(2, None, &edit);
        let graph = b.build();
        assert_eq!(graph.windows.len(), 1, "句柄已见归并到同一 win 节点");
        assert_eq!(graph.windows[0].handle, 100);
        let win = window_id(100);
        assert!(
            graph.associations.iter().any(|e| e.kind == AssociationKind::ChildOf
                && e.from == control_id(&edit)
                && e.to == win),
            "后到控件也指向同一 win 节点: {:?}",
            graph.associations
        );
        assert!(
            graph.associations.iter().any(|e| e.kind == AssociationKind::ChildOf
                && e.from == control_id(&toolbar)
                && e.to == win)
        );
    }

    /// P2-3 防误并：同标题、同进程但句柄不同的控件分属两个顶层窗口
    /// （桌面常态：两个「无标题 - 记事本」）——各开各的 win 节点，
    /// 窗口级归属不失真（宁可稀释也不误并）。
    #[test]
    fn ingest_does_not_merge_same_title_same_process_windows() {
        let mut b = ControlGraphBuilder::new();
        let mut first = click_control("a");
        first.handle = 100;
        let mut second = click_control("b");
        second.handle = 200;
        b.ingest(1, None, &first);
        b.ingest(2, None, &second);
        let graph = b.build();
        assert_eq!(graph.windows.len(), 2, "同 (title, proc) 新句柄不并入首见窗口");
        assert_eq!(graph.windows[0].handle, 100);
        assert_eq!(graph.windows[1].handle, 200);
        // child-of 边各挂各的窗口。
        assert!(
            graph.associations.iter().any(|e| e.kind == AssociationKind::ChildOf
                && e.from == control_id(&first)
                && e.to == window_id(100)),
            "{:?}",
            graph.associations
        );
        assert!(
            graph.associations.iter().any(|e| e.kind == AssociationKind::ChildOf
                && e.from == control_id(&second)
                && e.to == window_id(200))
        );
    }

    /// 标题未知（空串）时不归并：无法判同源，宁可逐句柄建节点也不误并。
    #[test]
    fn ingest_keeps_separate_win_nodes_when_title_unknown() {
        let mut b = ControlGraphBuilder::new();
        let mut a = click_control("a");
        a.handle = 100;
        a.window_title = String::new();
        let mut c = click_control("b");
        c.handle = 200;
        c.window_title = String::new();
        b.ingest(1, None, &a);
        b.ingest(2, None, &c);
        let graph = b.build();
        assert_eq!(graph.windows.len(), 2, "标题未知不归并（防误并不同窗口）");
    }

    /// 同名标题但不同进程：归并键含进程名，各自独立成节点。
    #[test]
    fn ingest_does_not_merge_same_title_across_processes() {
        let mut b = ControlGraphBuilder::new();
        let mut a = click_control("a");
        a.handle = 100;
        let mut c = click_control("b");
        c.handle = 200;
        c.process_name = "explorer.exe".into();
        b.ingest(1, None, &a);
        b.ingest(2, None, &c);
        let graph = b.build();
        assert_eq!(graph.windows.len(), 2, "(title, proc) 不同则不并");
    }

    #[test]
    fn associate_is_deduped_and_ingest_observation_wires_label_for() {
        let tree = SynthNode::branch(
            control("Window", "w", rect(0.0, 0.0, 800.0, 600.0)),
            vec![
                crate::harness::snapshot::synth::SynthNode::leaf(
                    control("Text", "名称", rect(10.0, 100.0, 60.0, 24.0)),
                ),
                crate::harness::snapshot::synth::SynthNode::leaf(
                    control("Edit", "", rect(10.0, 100.0, 300.0, 24.0)),
                ),
            ],
        );
        let probe = SyntheticProbe::new(tree, sample_windows());
        let obs = probe.control_at(20.0, 110.0).expect("Edit 命中");
        assert!(obs.label.is_some());

        let mut b = ControlGraphBuilder::new();
        b.ingest_observation(5, Some("step-1"), &obs);
        b.ingest_observation(6, Some("step-1"), &obs); // 重复观测去重。
        // associate 手工登记（dialog-of）+ 去重。
        b.associate("win:1".into(), "win:2".into(), AssociationKind::DialogOf);
        b.associate("win:1".into(), "win:2".into(), AssociationKind::DialogOf);
        let graph = b.build();
        let label = obs.label.as_ref().expect("label 关联");
        let label_edge = graph
            .associations
            .iter()
            .find(|e| e.kind == AssociationKind::LabelFor)
            .expect("label-for 边应产出");
        assert_eq!(label_edge.from, control_id(label), "label 控件已入图");
        assert_eq!(label_edge.to, control_id(&obs.control));
        // 观测总数：控件2（目标 + label）+ 窗口1 + 进程1；边 = label-for
        // + 2×ctrl→win + win→proc + step-acted-on + dialog-of = 6。
        assert_eq!(graph.controls.len(), 2, "label 控件一并入图（P2-2）");
        assert_eq!(graph.associations.len(), 6, "{:?}", graph.associations);
        assert_eq!(
            graph.associations.iter().filter(|e| e.kind == AssociationKind::DialogOf).count(),
            1
        );
        // P2-2 全图不变式：控件端点必有节点（无悬空边）；child-of 的
        // 窗口/进程端点必在对应节点表（win:2 为 dialog-of 前向引用，不查）。
        let ctrl_ids: HashSet<_> = graph.controls.iter().map(|n| n.id.clone()).collect();
        let win_ids: HashSet<_> = graph.windows.iter().map(|w| w.id.clone()).collect();
        let proc_ids: HashSet<_> = graph.processes.iter().map(|p| p.id.clone()).collect();
        for e in &graph.associations {
            for id in [&e.from, &e.to] {
                if id.starts_with("ctrl:") {
                    assert!(ctrl_ids.contains(id), "悬空控件端点 {id}: {e:?}");
                }
            }
            if e.kind == AssociationKind::ChildOf {
                if e.to.starts_with("win:") {
                    assert!(win_ids.contains(&e.to), "child-of 窗口端点缺节点: {e:?}");
                }
                if e.to.starts_with("proc:") {
                    assert!(proc_ids.contains(&e.to), "child-of 进程端点缺节点: {e:?}");
                }
            }
        }
    }

    /// P2-4：无句柄元素（网页内容恒 handle=0）的第二定位维度 = 树路径。
    /// 同 type/name/automationId/className 但 path 不同 → 不同指纹、
    /// 不同节点，event_seqs 不合并。
    #[test]
    fn fingerprint_path_dimension_separates_handleless_twins() {
        let make = |path: &str| {
            let mut c = click_control("删除");
            c.handle = 0; // 无句柄元素（HTML 内容）。
            c.path = Some(path.into());
            c
        };
        let a = make("Window[页]/Pane[列表1]/Button[删除]");
        let b = make("Window[页]/Pane[列表2]/Button[删除]");
        assert_ne!(control_fingerprint(&a), control_fingerprint(&b), "path 参与指纹");
        assert_ne!(control_id(&a), control_id(&b));

        let mut builder = ControlGraphBuilder::new();
        builder.ingest(1, None, &a);
        builder.ingest(2, None, &a); // 同一控件重复观测：seq 合并。
        builder.ingest(3, None, &b);
        let graph = builder.build();
        assert_eq!(graph.controls.len(), 2, "path 维度隔离，不碰撞去重");
        let a_seqs = graph.controls.iter().find(|n| n.id == control_id(&a)).unwrap();
        assert_eq!(a_seqs.event_seqs, vec![1, 2], "同控件 seq 归并");
        let b_seqs = graph.controls.iter().find(|n| n.id == control_id(&b)).unwrap();
        assert_eq!(b_seqs.event_seqs, vec![3], "双胞胎控件 seq 不合并");
    }

    #[test]
    fn empty_builder_skips_as_empty_graph() {
        let b = ControlGraphBuilder::new();
        assert!(b.is_empty());
        drop(b.build());
        // 有控件后不再为空。
        let mut b2 = ControlGraphBuilder::new();
        b2.ingest(1, None, &click_control("提交"));
        assert!(!b2.is_empty());
    }

    /* --- associate debug 断言（P3-13；debug profile 专属） -------------------- */

    /// 一跳环（自环）：debug 构建下 panic（release 不生效——debug_assert
    /// 语义，`cargo test` 缺省即 debug profile）。五种边均为跨端点语义，
    /// from == to 必是登记方 bug。`--release` 下断言被编译掉，测试同步
    /// 跳过（否则 should_panic 必假红）。
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "一跳环（自环）")]
    fn associate_rejects_self_edge_in_debug_builds() {
        let mut b = ControlGraphBuilder::new();
        let c = click_control("提交");
        b.ingest(1, None, &c);
        let id = control_id(&c);
        b.associate(id.clone(), id, AssociationKind::FocusChain);
    }

    /// 悬空 `ctrl:` 端点（P2-2 既有不变式的触发路径）：未入图的控件 id
    /// 登记 label-for，debug 构建下 panic。`win:` 前向引用合法（对照，
    /// 同一测试文件中 dialog-of 测试已覆盖）。
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "悬空边")]
    fn associate_rejects_dangling_ctrl_endpoint_in_debug_builds() {
        let mut b = ControlGraphBuilder::new();
        b.associate(
            "ctrl:0000000000000000".into(),
            "win:1".into(),
            AssociationKind::LabelFor,
        );
    }

    #[test]
    fn fingerprint_ownership_locates_target_in_scene_tree() {
        let scene = SceneSnapshot {
            at: 0,
            window: click_control("主窗口"),
            target_control: None,
            tree_pruned: vec![
                SceneNode {
                    path: "0".into(),
                    control_type: "Pane".into(),
                    name: "表单".into(),
                    automation_id: None,
                    class_name: Some("Pane".into()),
                    enabled: Some(true),
                },
                SceneNode {
                    path: "0/2".into(),
                    control_type: "Button".into(),
                    name: "提交".into(),
                    automation_id: Some("SubmitBtn".into()),
                    class_name: Some("Button".into()),
                    enabled: Some(true),
                },
            ],
            dialogs: Vec::new(),
            screenshots: Vec::new(),
        };
        // 精确：四锚点全等。
        let mut target = click_control("提交");
        target.automation_id = "SubmitBtn".into();
        assert_eq!(FingerprintOwnership.locate(&scene, &target).as_deref(), Some("0/2"));
        // 找不到 → None。
        assert_eq!(FingerprintOwnership.locate(&scene, &click_control("不存在")), None);
        // 名称空串但 AutomationId 命中 → 走兜底分支命中（AutomationId 是强锚点）。
        assert_eq!(
            FingerprintOwnership.locate(
                &scene,
                &ControlRef { name: String::new(), automation_id: "SubmitBtn".into(), ..click_control("提交") }
            )
            .as_deref(),
            Some("0/2")
        );
        // 兜底：仅 AutomationId 命中（名称漂移场景）。
        assert_eq!(
            FingerprintOwnership.locate(
                &scene,
                &ControlRef { name: "OK".into(), automation_id: "SubmitBtn".into(), ..click_control("提交") }
            )
            .as_deref(),
            Some("0/2")
        );
    }
}
