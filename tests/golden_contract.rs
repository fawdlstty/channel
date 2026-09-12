#![cfg(feature = "harness")]
//! golden 契约对齐（design.md §19/§22.5；双轨的 harness 侧）。
//!
//! 同一份 `fixtures/harness-contract-golden.json`：
//! - 本测试断言 **serde 强类型往返**（反序列化 → 序列化 == 原文 Value）；
//! - app 侧 `common/harness.test.ts` 断言 **zod 解析**（vitest）。
//!
//! 双侧字段名/枚举字面量漂移即测试红。改 fixture 必须同步两侧契约。
//!
//! **v1.15 边界（用户要求 U）**：`recordEventUi`/`recordEventBrowser`/
//! `relocation` 三段随录制/浏览器/LLM 能力迁至 WorkRecorder 仓的
//! `wr-harness-ext` crate，其 serde 断言由该 crate 的
//! `tests/golden_contract.rs` 承接（读同一份 fixture 镜像）；本文件只锁
//! 通用底座段。

use channel::harness::graph::{
    control_fingerprint, process_id, step_node_id, window_id, ControlGraphBuilder,
};
use channel::harness::snapshot::synth::{SyntheticProbe, SynthNode};
use channel::harness::snapshot::{ControlObservation, ControlProbe as _};
use channel::harness::store::{
    AssociationKind, ControlGraph, ControlNode, ControlRect, ControlRef, ProcessNode, SceneNode,
    SceneSnapshot, UiAssociation, CONTROL_GRAPH_VERSION,
};
use serde_json::Value;

fn fixture() -> Value {
    let text = include_str!("../fixtures/harness-contract-golden.json");
    serde_json::from_str(text).expect("golden fixture 必须是合法 JSON")
}

/// 往返断言：T ↔ fixture[key] 双向稳定。
fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
    key: &str,
) -> T {
    let raw = fixture().get(key).unwrap_or_else(|| panic!("fixture 缺 {key}")).clone();
    let parsed: T = serde_json::from_value(raw.clone()).unwrap_or_else(|e| panic!("{key} 反序列化失败: {e}"));
    let written = serde_json::to_value(&parsed).unwrap_or_else(|e| panic!("{key} 序列化失败: {e}"));
    assert_eq!(written, raw, "{key} 序列化形态漂移（字段名/camelCase/缺省字段）");
    parsed
}

#[test]
fn golden_control_ref_roundtrips() {
    let control: ControlRef = roundtrip("controlRef");
    assert_eq!(control.control_type, "Button");
    assert_eq!(control.name, "提交");
    assert_eq!(control.enabled, Some(true));
    assert_eq!(control.rect, ControlRect { x: 480.0, y: 360.0, width: 88.0, height: 28.0 });
    // 最小形态：可选字段缺席仍可读回（向后兼容读旧文档）。
    let minimal: ControlRef = roundtrip("controlRefMinimal");
    assert_eq!(minimal.enabled, None);
    assert_eq!(minimal.path, None);
}

#[test]
fn golden_ui_associations_cover_all_kinds() {
    let edges: Vec<UiAssociation> = roundtrip("uiAssociations");
    let kinds: Vec<AssociationKind> = edges.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            AssociationKind::ChildOf,
            AssociationKind::LabelFor,
            AssociationKind::DialogOf,
            AssociationKind::FocusChain,
            AssociationKind::StepActedOn,
        ],
        "五种关联边字面量在 golden 中逐一锁定"
    );
}

#[test]
fn golden_scene_snapshot_roundtrips() {
    let scene: SceneSnapshot = roundtrip("sceneSnapshot");
    assert_eq!(scene.window.control_type, "Window");
    assert_eq!(scene.target_control.as_ref().unwrap().automation_id, "SubmitBtn");
    assert_eq!(scene.tree_pruned.len(), 2);
    assert_eq!(scene.tree_pruned[0].path, "0");
    assert_eq!(scene.dialogs.len(), 1);
    assert_eq!(scene.screenshots, vec!["frames/scene-0001.png".to_string()]);
}

#[test]
fn golden_control_graph_roundtrips() {
    let graph: ControlGraph = roundtrip("controlGraph");
    assert_eq!(graph.version, CONTROL_GRAPH_VERSION);
    assert_eq!(graph.processes[0].id, "proc:excel.exe");
    assert_eq!(graph.windows[0].id, "win:197296");
    assert_eq!(graph.controls[0].event_seqs, vec![8]);
    assert_eq!(graph.associations.len(), 3);
    // 与 builder 的节点 id 约定一致（形态锁定：win:<handle> / proc:<name>）。
    assert_eq!(window_id(197_296), graph.windows[0].id);
    assert_eq!(process_id("excel.exe"), graph.processes[0].id);
}

#[test]
fn golden_graph_is_reproducible_from_builder() {
    // golden 的 controlGraph 必须能由「一次点击观测」经 builder 复现
    // （同指纹 → 同 ctrl:<hex> 节点 id —— golden 中的指纹 hex 即真实产物）。
    let control: ControlRef = roundtrip("controlRef");
    let mut builder = ControlGraphBuilder::new().session_id("20260907-000000-deadbeef");
    builder.ingest(8, Some("step-1"), &control);
    let built = builder.build();
    let golden: ControlGraph = roundtrip("controlGraph");
    assert_eq!(built, golden, "builder 产物必须逐字段等于 golden（含 ctrl 指纹 id）");
    assert_eq!(step_node_id("step-1"), "step:step-1");
}

#[test]
fn golden_ctrl_ids_match_real_fingerprints() {
    // golden fixture 里的 ctrl:<hex> 不是随手写的：由指纹函数计算锁定。
    let control: ControlRef = roundtrip("controlRef");
    let expected = format!("ctrl:{:016x}", control_fingerprint(&control));
    let golden: ControlGraph = roundtrip("controlGraph");
    assert_eq!(golden.controls[0].id, expected);
    assert_eq!(golden.associations[0].from, expected);
}

/// serve 错误响应的 code 键形态（upgrade.md §10.3 加法式）：样本与
/// Error::ActuationLocked 的 Display/code 双向一致（错误文案即 wire 契约）。
#[test]
fn golden_error_code_sample_matches_error_code() {
    let sample = fixture().get("errorCodeSample").cloned().unwrap();
    let sample: Value = sample;
    assert_eq!(sample["ok"], serde_json::json!(false));
    assert_eq!(sample["code"], serde_json::json!("actuation-locked"));
    // 文案与 code 均来自 Error 枚举同一来源（无第二事实点）。
    let error = channel::harness::Error::ActuationLocked;
    assert_eq!(sample["error"], serde_json::json!(error.to_string()));
    assert_eq!(sample["code"], serde_json::json!(error.code().unwrap()));
}

#[test]
fn synth_probe_observe_shape_matches_golden_control() {
    // 合成树观测产出的 ControlRef serde 形态 == golden 形态（同字段集）。
    let root = SynthNode::window(
        ControlRef {
            handle: 1,
            control_type: "Window".into(),
            name: "w".into(),
            automation_id: String::new(),
            class_name: String::new(),
            window_title: "w".into(),
            process_name: "p.exe".into(),
            rect: ControlRect { x: 0.0, y: 0.0, width: 100.0, height: 100.0 },
            enabled: Some(true),
            path: None,
        },
        vec![SynthNode::leaf(ControlRef {
            handle: 1,
            control_type: "Button".into(),
            name: "提交".into(),
            automation_id: "SubmitBtn".into(),
            class_name: "Button".into(),
            window_title: "w".into(),
            process_name: "p.exe".into(),
            rect: ControlRect { x: 10.0, y: 10.0, width: 50.0, height: 20.0 },
            enabled: Some(true),
            path: None,
        })],
    );
    let probe = SyntheticProbe::new(root, Vec::new());
    let obs: ControlObservation = probe.control_at(20.0, 15.0).expect("命中");
    let control_node = ControlNode {
        id: channel::harness::graph::control_id(&obs.control),
        control: obs.control.clone(),
        event_seqs: vec![8],
    };
    let value = serde_json::to_value(&control_node).unwrap();
    assert!(value.get("control").and_then(|c| c.get("controlType")).is_some());
    // label 关联形态（label-for 边）序列化字面量。
    let edge = UiAssociation {
        from: "ctrl:a".into(),
        to: "ctrl:b".into(),
        kind: AssociationKind::LabelFor,
    };
    assert_eq!(
        serde_json::to_value(&edge).unwrap(),
        serde_json::json!({ "from": "ctrl:a", "to": "ctrl:b", "kind": "label-for" })
    );
}

/// 占位防误删（ProcessNode/SceneNode 在本文件其它测试间接锁定形态）。
#[test]
fn node_shapes_stay_minimal() {
    let process = ProcessNode { id: "proc:p".into(), name: "p".into() };
    assert_eq!(serde_json::to_value(&process).unwrap(), serde_json::json!({ "id": "proc:p", "name": "p" }));
    let node = SceneNode {
        path: "0".into(),
        control_type: "Pane".into(),
        name: "n".into(),
        automation_id: None,
        class_name: None,
        enabled: None,
    };
    let value = serde_json::to_value(&node).unwrap();
    assert_eq!(value, serde_json::json!({ "path": "0", "controlType": "Pane", "name": "n" }));
}
