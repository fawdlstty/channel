//! 合成控件树探测（[`ControlProbe`] 的纯逻辑实现）。
//!
//! 用途：harness 全链路单测（graph/tools/serve/store）与 app 侧注入 mock——
//! 不碰真实 UIA。命中/焦点/label 关联/树转储的**语义**在此定义并被单测
//! 锁定，真实 UIA 实现（`uia.rs`）对齐同一语义：
//!
//! - 命中：包含该点的**最深**节点；同深度的兄弟重叠时取**兄弟序靠后**
//!   者（模拟 UIA 的 topmost/last-child 语义）；
//! - label 关联：命中节点的**前一个 Text 兄弟**（非空 Name），向前回看
//!   至多 [`LABEL_MAX_PREV_SIBLINGS`] 个兄弟；
//! - `path`：祖先链的 `ControlType[name]` 以 `/` 连接（含自身），祖先深度
//!   至多 [`PATH_MAX_DEPTH`]（更远的根方向祖先截去，保最近链）；
//! - `dump_tree`：根起**前序 DFS**（栈实现，兄弟按正序访问）、`path` 为
//!   `/` 连接的子序号；深度上限 = 剪枝语义（达限节点不下钻子树，其余
//!   分支照常遍历），节点数上限 = 收集预算（用尽即停收集，已产出的
//!   浅层分支不受影响）。
//!
//! 语义上限（P3-7）以本模块为**定义者**：`uia.rs` 引用同一常量对齐，
//! mock 测试通过 ⇔ 实机同语义。

use super::{ControlObservation, ControlProbe, WindowRef};
use crate::harness::store::{ControlRef, SceneNode};

/// label 启发式向前回看的最兄弟步数（语义上限，`uia.rs` 引用同一常量）。
/// 取 2：表单布局中「标签紧邻控件」是常态，回看过远易把分组标题等无关
/// 文本误当标签；2 步内无「非空 Name 的 Text」兄弟即放弃。
pub const LABEL_MAX_PREV_SIBLINGS: usize = 2;

/// `ControlRef.path` 的最大祖先深度（语义上限，`uia.rs` 引用同一常量）。
/// 取 8：与 tools 侧 `dump_tree` 缺省深度钳制（8）同数量级——防深树巨长
/// 路径，同时保住定位所需的近链语义。
pub const PATH_MAX_DEPTH: usize = 8;

/// 合成树节点。
#[derive(Debug, Clone, PartialEq)]
pub struct SynthNode {
    pub control: ControlRef,
    pub children: Vec<SynthNode>,
    /// 顶层窗口标记（dump_tree/语义无差，仅供构造表达）。
    pub is_window: bool,
    /// 焦点标记（`SyntheticProbe::focused` 命中该节点）。
    pub is_focused: bool,
}

impl SynthNode {
    pub fn leaf(control: ControlRef) -> Self {
        Self { control, children: Vec::new(), is_window: false, is_focused: false }
    }

    /// 带焦点标记的叶子（键入段首解析场景）。
    pub fn focused_leaf(control: ControlRef) -> Self {
        Self { is_focused: true, ..Self::leaf(control) }
    }

    pub fn branch(control: ControlRef, children: Vec<SynthNode>) -> Self {
        Self { control, children, is_window: false, is_focused: false }
    }

    pub fn window(control: ControlRef, children: Vec<SynthNode>) -> Self {
        Self { is_window: true, ..Self::branch(control, children) }
    }
}

/// 合成探测：控件树 + 顶层窗口清单（首项 = 前台）。
#[derive(Debug, Clone)]
pub struct SyntheticProbe {
    root: SynthNode,
    windows: Vec<WindowRef>,
}

impl SyntheticProbe {
    pub fn new(root: SynthNode, windows: Vec<WindowRef>) -> Self {
        Self { root, windows }
    }
}

/// 命中候选：节点 + 祖先链（供 path 标注）+ label 查找所需的兄弟上下文。
struct Hit<'a> {
    node: &'a SynthNode,
    ancestors: Vec<&'a SynthNode>,
    depth: usize,
    label: Option<ControlRef>,
}

fn contains(r: &crate::harness::store::ControlRect, x: f64, y: f64) -> bool {
    r.width > 0.0
        && r.height > 0.0
        && x >= r.x
        && x < r.x + r.width
        && y >= r.y
        && y < r.y + r.height
}

/// 前一个 Text 兄弟（非空 Name）→ label 引用；从最近兄弟起回看，至多
/// [`LABEL_MAX_PREV_SIBLINGS`] 步（与 uia 实现的 TreeWalker 语义对齐）。
fn label_of(siblings: &[SynthNode], index: usize) -> Option<ControlRef> {
    siblings[..index]
        .iter()
        .rev()
        .take(LABEL_MAX_PREV_SIBLINGS)
        .find_map(|sib| {
            (sib.control.control_type == "Text" && !sib.control.name.is_empty())
                .then(|| sib.control.clone())
        })
}

fn annotate(control: &ControlRef, ancestors: &[&SynthNode], node: &SynthNode) -> ControlRef {
    let mut out = control.clone();
    // 祖先链至多 PATH_MAX_DEPTH 段（保最近链，根方向截去——与 uia 的
    // build_path 截断语义一致）。
    let base = ancestors.len().saturating_sub(PATH_MAX_DEPTH);
    let mut segments: Vec<String> = ancestors[base..]
        .iter()
        .map(|a| format!("{}[{}]", a.control.control_type, a.control.name))
        .collect();
    segments.push(format!("{}[{}]", node.control.control_type, node.control.name));
    out.path = Some(segments.join("/"));
    out
}

impl ControlProbe for SyntheticProbe {
    fn control_at(&self, x: f64, y: f64) -> Option<ControlObservation> {
        let mut best: Option<Hit<'_>> = None;
        fn walk<'a>(
            node: &'a SynthNode,
            siblings: &'a [SynthNode],
            index: usize,
            ancestors: &[&'a SynthNode],
            x: f64,
            y: f64,
            best: &mut Option<Hit<'a>>,
        ) {
            if !contains(&node.control.rect, x, y) {
                return; // 不含点：剪枝（合成树父矩形恒包含子矩形）。
            }
            let depth = ancestors.len();
            let label = label_of(siblings, index);
            let better = match &*best {
                // 更深者优先；同深取后者（兄弟序靠后 = topmost）。
                Some(cur) => depth >= cur.depth,
                None => true,
            };
            if better {
                *best = Some(Hit { node, ancestors: ancestors.to_vec(), depth, label });
            }
            for (i, child) in node.children.iter().enumerate() {
                let mut next = ancestors.to_vec();
                next.push(node);
                walk(child, &node.children, i, &next, x, y, best);
            }
        }
        walk(&self.root, std::slice::from_ref(&self.root), 0, &[], x, y, &mut best);
        let hit = best?;
        let control = annotate(&hit.node.control, &hit.ancestors, hit.node);
        Some(ControlObservation { control, label: hit.label })
    }

    fn focused(&self) -> Option<ControlObservation> {
        // 前序搜索焦点节点；命中路径的祖先链经 `ancestors` 累积
        //（含焦点节点自身）。返回 Some(label) = 已命中。
        fn collect<'a>(
            node: &'a SynthNode,
            siblings: &'a [SynthNode],
            index: usize,
            ancestors: &mut Vec<&'a SynthNode>,
        ) -> Option<Option<ControlRef>> {
            if node.is_focused {
                ancestors.push(node);
                return Some(label_of(siblings, index));
            }
            ancestors.push(node);
            for (i, child) in node.children.iter().enumerate() {
                if let Some(label) = collect(child, &node.children, i, ancestors) {
                    return Some(label);
                }
            }
            ancestors.pop();
            None
        }
        let mut ancestors: Vec<&SynthNode> = Vec::new();
        let label = collect(&self.root, std::slice::from_ref(&self.root), 0, &mut ancestors)?;
        let node = *ancestors.last()?;
        let control = annotate(&node.control, &ancestors[..ancestors.len() - 1], node);
        Some(ControlObservation { control, label })
    }

    fn foreground_window(&self) -> Option<WindowRef> {
        self.windows.first().cloned()
    }

    fn list_windows(&self) -> Vec<WindowRef> {
        self.windows.clone()
    }

    fn dump_tree(&self, max_depth: u32, max_nodes: usize) -> Vec<SceneNode> {
        let mut out = Vec::new();
        if max_depth == 0 || max_nodes == 0 {
            return out;
        }
        let mut queue: Vec<(&SynthNode, String, u32)> = vec![(&self.root, "0".into(), 1)];
        while let Some((node, path, depth)) = queue.pop() {
            let control = &node.control;
            out.push(SceneNode {
                path: path.clone(),
                control_type: control.control_type.clone(),
                name: control.name.clone(),
                automation_id: (!control.automation_id.is_empty()).then(|| control.automation_id.clone()),
                class_name: (!control.class_name.is_empty()).then(|| control.class_name.clone()),
                enabled: control.enabled,
            });
            if out.len() >= max_nodes {
                out.truncate(max_nodes);
                break;
            }
            if depth < max_depth {
                for (i, child) in node.children.iter().enumerate().rev() {
                    queue.push((child, format!("{path}/{i}"), depth + 1));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::snapshot::testutil::{control, rect};

    #[test]
    fn contains_is_half_open_and_rejects_empty_rects() {
        let r = rect(10.0, 10.0, 50.0, 20.0);
        assert!(contains(&r, 10.0, 10.0));
        assert!(contains(&r, 59.9, 29.9));
        assert!(!contains(&r, 60.0, 10.0), "右边界开区间");
        assert!(!contains(&r, 10.0, 30.0), "下边界开区间");
        assert!(!contains(&rect(0.0, 0.0, 0.0, 0.0), 0.0, 0.0), "空矩形不含点");
    }

    #[test]
    fn dump_tree_paths_are_child_index_chains() {
        let root = SynthNode::window(
            control("Window", "w", rect(0.0, 0.0, 100.0, 100.0)),
            vec![
                SynthNode::leaf(control("Button", "a", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Edit", "b", rect(0.0, 0.0, 10.0, 10.0))),
            ],
        );
        let p = SyntheticProbe::new(root, Vec::new());
        let paths: Vec<String> = p.dump_tree(8, 100).into_iter().map(|n| n.path).collect();
        assert_eq!(paths, vec!["0", "0/0", "0/1"]);
    }

    /// 回归锁定（uia.rs 旧实现对齐缺陷）：深度触限 = 只剪该子树，
    /// 不得终止整趟遍历而连带丢弃其他浅层分支。
    #[test]
    fn dump_tree_depth_cap_prunes_subtree_only_keeps_other_branches() {
        // Window(0)
        // ├─ Pane(0/0)（depth 2 触限，其子树应被剪）
        // │  └─ Button deep(0/0/0)
        // └─ Button shallow(0/1)（Pane 之后的浅层兄弟，不得被连带丢弃）
        let root = SynthNode::window(
            control("Window", "w", rect(0.0, 0.0, 100.0, 100.0)),
            vec![
                SynthNode::branch(
                    control("Pane", "p", rect(0.0, 0.0, 50.0, 100.0)),
                    vec![SynthNode::leaf(control("Button", "deep", rect(0.0, 0.0, 10.0, 10.0)))],
                ),
                SynthNode::leaf(control("Button", "shallow", rect(60.0, 0.0, 10.0, 10.0))),
            ],
        );
        let p = SyntheticProbe::new(root, Vec::new());
        let names: Vec<String> = p.dump_tree(2, 100).into_iter().map(|n| n.name).collect();
        assert_eq!(
            names,
            vec!["w", "p", "shallow"],
            "深度触限只剪子树，浅层兄弟分支保留"
        );
    }

    /// 节点预算语义：用尽即停收集，但预算内产出的顺序仍是前序 DFS
    /// （兄弟正序），且不因预算截断破坏已产出节点。
    #[test]
    fn dump_tree_node_budget_stops_collection_in_dfs_order() {
        let root = SynthNode::window(
            control("Window", "w", rect(0.0, 0.0, 100.0, 100.0)),
            vec![
                SynthNode::leaf(control("Button", "a", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Button", "b", rect(20.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Button", "c", rect(40.0, 0.0, 10.0, 10.0))),
            ],
        );
        let p = SyntheticProbe::new(root, Vec::new());
        let names: Vec<String> = p.dump_tree(8, 3).into_iter().map(|n| n.name).collect();
        assert_eq!(names, vec!["w", "a", "b"], "预算 3：根 + 前两个兄弟（DFS 正序）");
    }

    /// 语义上限锁定（P3-7）：label 回看至多 LABEL_MAX_PREV_SIBLINGS 个前
    /// 兄弟——2 步内的 Text 命中、更远的 Text 不再命中（与 uia 的
    /// TreeWalker 回看语义一致；旧行为无上限，属语义收窄）。
    #[test]
    fn label_lookback_is_capped_to_const_siblings() {
        let build = |siblings: Vec<SynthNode>| {
            let hit = SynthNode::focused_leaf(control("Edit", "", rect(0.0, 0.0, 10.0, 10.0)));
            let all = siblings.into_iter().chain(std::iter::once(hit)).collect();
            SynthNode::window(control("Window", "w", rect(0.0, 0.0, 100.0, 100.0)), all)
        };
        let p = SyntheticProbe::new(
            build(vec![
                SynthNode::leaf(control("Group", "g1", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Text", "近处", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Group", "g2", rect(0.0, 0.0, 10.0, 10.0))),
            ]),
            Vec::new(),
        );
        let obs = p.focused().expect("焦点节点");
        assert_eq!(obs.label.as_ref().expect("2 步内 Text 命中").name, "近处");

        let p = SyntheticProbe::new(
            build(vec![
                SynthNode::leaf(control("Text", "远处", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Group", "g1", rect(0.0, 0.0, 10.0, 10.0))),
                SynthNode::leaf(control("Group", "g2", rect(0.0, 0.0, 10.0, 10.0))),
            ]),
            Vec::new(),
        );
        let obs = p.focused().expect("焦点节点");
        assert!(obs.label.is_none(), "第 3 个前兄弟的 Text 超出回看上限，不再命中");
    }

    /// 语义上限锁定（P3-7）：path 祖先深度至多 PATH_MAX_DEPTH——超深树的
    /// 路径保**最近**链（8 祖先 + 自身），根方向祖先截去（与 uia 的
    /// build_path 截断语义一致；旧行为无上限，属语义收窄）。
    #[test]
    fn path_depth_is_capped_to_const_ancestors() {
        // 12 层链：Window + 10 层 Pane + 叶 Edit（全部含点，命中叶子）。
        let mut node = SynthNode::focused_leaf(control("Edit", "hit", rect(0.0, 0.0, 1.0, 1.0)));
        for i in 0..10 {
            node = SynthNode::branch(
                control("Pane", &format!("p{i}"), rect(0.0, 0.0, 100.0, 100.0)),
                vec![node],
            );
        }
        node = SynthNode::window(
            control("Window", "w", rect(0.0, 0.0, 100.0, 100.0)),
            vec![node],
        );
        let p = SyntheticProbe::new(node, Vec::new());
        let obs = p.control_at(0.5, 0.5).expect("全链含点，叶子命中");
        let path = obs.control.path.expect("path 已标注");
        let segments: Vec<&str> = path.split('/').collect();
        assert_eq!(segments.len(), PATH_MAX_DEPTH + 1, "8 祖先 + 自身");
        // 嵌套使 p9 在最外：祖先序 = [Window, p9..p0]（11 个），截去最外
        // 3 个（Window/p9/p8），路径顶 = p7。
        assert_eq!(segments[0], "Pane[p7]", "根方向超限祖先截去，p7 成为路径顶");
        assert_eq!(segments[PATH_MAX_DEPTH], "Edit[hit]", "链尾 = 自身");
        assert!(!path.contains("Window["), "根方向超限祖先不进路径");
        assert!(!path.contains("p9[") && !path.contains("p8["), "最外两层 Pane 同样截去");
    }
}
