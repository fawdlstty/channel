//! H3 原子工具面（design.md §22.4；GenericAgent 式极小工具注册表）。
//!
//! **分层授权工具集**（v1.11，用户要求 Q——§22.8-3 方案乙；感知默认可用，
//! 操作类仅授权进程可用，§22.7）：
//!
//! | 工具 | 授权 | 参数 | 产物 |
//! |---|---|---|---|
//! | `ping` | 公开 | - | `{"pong":true,"ts":ms}` |
//! | `version` | 公开 | - | 名称/版本/协议版本/感知与操作面可用性 |
//! | `list_windows` | 感知 | - | 可见顶层窗口清单 |
//! | `get_focus` | 感知 | - | 焦点控件观测 |
//! | `get_control` | 感知 | `{x,y}` | 坐标处控件观测 |
//! | `get_value` | 感知 | `{x,y}` | 坐标处控件值（ValuePattern 读值，Name 兜底） |
//! | `dump_tree` | 感知 | `{maxDepth?,maxNodes?}` | 前台窗口控件树（裁剪） |
//! | `capture_scene` | 感知 | `{x?,y?,maxDepth?,maxNodes?}` | 异常期 SceneSnapshot 组装（§22.5） |
//! | `capture_screen` | 感知 | `{x?,y?,width?,height?}` | 截图（四参全省 = 整虚拟屏；全给 = 区域，P3-12；`pngBase64/width/height`，不落盘，v1.13） |
//! | `click` | **操作** | `{x,y,button?,clicks?}` | 点击回执 |
//! | `drag` | **操作** | `{fromX,fromY,toX,toY,via?,durationMs?,steps?}` | 拖拽回执 |
//! | `type_text` | **操作** | `{text,intervalMs?}` | 键入回执（UNICODE 注入，中文可用） |
//! | `key` | **操作** | `{combo}` | 组合键回执（`ctrl+s` 形态） |
//! | `set_value` | **操作** | `{value,x,y}` | ValuePattern 直写回执 |
//!
//! 探测不可用（无 UIA/非 Windows）时感知类工具返回 `Err`（serve 层转
//! `ok:false` 响应）；操作类仅在注册表显式持有 [`crate::harness::actuate::Actuator`]
//! 时可用——未授权调用错误文案指明 `--actuation` / `HARNESS_ACTUATION=1`
//! 授权开关；`ping`/`version` 不依赖任何探测，恒可用。
//!
//! **脱敏契约（P2-6 收口，upgrade.md §4.5/R3）**：注册表可经
//! [`ToolRegistry::with_redactor`] 注入 [`crate::harness::store::Redactor`]，覆盖
//! **全部**感知 op 的出参自由文本路径——`capture_scene` / `get_value`
//! / `list_windows`（窗口标题）/ `get_focus`（control/label，经
//! [`crate::harness::store::redact_control_ref`]）/ `dump_tree`（节点 name）。
//! （bin 的 `--redact` / `HARNESS_REDACT` 即经此注入，并同时覆盖 serve
//! 焦点事件的自由文本字段。）
//! **未注入 redactor 时（缺省），stdout 出参含未脱敏文本**——窗口标题、
//! 控件文本、读值明文离开本进程，消费者必须按 design §22.7 兜底做敏感扫描。
//!
//! **v1.15 边界（用户要求 U）**：`relocate_control` op（LLM 桥）与
//! `browser_*` 七 op（CDP 直录）已随录制/浏览器/LLM 能力迁至 WorkRecorder
//! 仓的 `wr-harness-ext` crate——注册表不再承载事件汇/LLM/浏览器三态。

use std::sync::Arc;

use serde_json::{json, Value};

use crate::harness::actuate::{Actuator, MouseButton};
use crate::harness::snapshot::{ControlProbe, SceneCaptureOptions};
use crate::harness::store::{redact_control_ref, Redactor};
use crate::harness::Error;

/// 未授权调用操作类 op 的错误文案（指明授权开关；单测锁定）。
pub const ACTUATION_LOCKED_MESSAGE: &str =
    "操作类工具未授权：需以 harness --actuation 启动（或环境变量 HARNESS_ACTUATION=1）";

/// 感知类工具注册表条目（名称 + 一句话职责）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: &'static str,
    pub summary: &'static str,
}

/// 只读感知工具清单（§22.7「感知默认可用」的机器可读形态；
/// `ping`/`version` 属协议层非感知工具，不在此列）。
pub const READ_ONLY_TOOLS: [ToolInfo; 7] = [
    ToolInfo { name: "list_windows", summary: "可见顶层窗口清单" },
    ToolInfo { name: "get_focus", summary: "当前键盘焦点控件" },
    ToolInfo { name: "get_control", summary: "屏幕坐标处的控件" },
    ToolInfo { name: "get_value", summary: "屏幕坐标处的控件值（ValuePattern 读值）" },
    ToolInfo { name: "dump_tree", summary: "前台窗口控件树（裁剪转储）" },
    ToolInfo { name: "capture_scene", summary: "异常期 Scene 快照（前台窗口+目标控件+裁剪树+同进程弹窗）" },
    ToolInfo { name: "capture_screen", summary: "截图（GDI BitBlt+WIC 编 PNG；pngBase64/width/height，不落盘；可选 x/y/width/height 区域参数，全省 = 整虚拟屏）" },
];

/// 操作类工具清单（v1.11 Q 方案乙：键鼠模拟五原语；仅授权进程注册）。
pub const ACTUATION_TOOLS: [ToolInfo; 5] = [
    ToolInfo { name: "click", summary: "坐标点击（左/右键，单击/双击）" },
    ToolInfo { name: "drag", summary: "按下→路径采样→抬起的一等拖拽手势" },
    ToolInfo { name: "type_text", summary: "键入文本（UNICODE 注入，中文可用；\\n=回车）" },
    ToolInfo { name: "key", summary: "组合键（ctrl+s 形态）" },
    ToolInfo { name: "set_value", summary: "坐标处控件 ValuePattern 直写" },
];

/// dump_tree 的缺省参数（防巨型树：默认 8 层 / 512 节点）。
pub const DUMP_TREE_DEFAULT_DEPTH: u32 = 8;
pub const DUMP_TREE_MAX_NODES_DEFAULT: usize = 512;
/// dump_tree 参数硬上限（即使调用方传更大值）。
pub const DUMP_TREE_MAX_DEPTH_CAP: u32 = 16;
pub const DUMP_TREE_MAX_NODES_CAP: usize = 10_000;

/// capture_screen 区域参数的宽/高上限（P3-12；防误传巨幅——10000px 已
/// 覆盖现有最大单屏与常见多屏虚拟屏组合，超出视为调用方 bug）。
pub const CAPTURE_SCREEN_MAX_DIMENSION: i32 = 10_000;

/// 工具注册表：感知工具 → [`ControlProbe`]；操作工具 → [`Actuator`]（仅
/// 授权进程持有，§22.7 授权门）；可选 [`Redactor`]（出参脱敏，P2-6 收口
/// 后覆盖全部感知 op 的自由文本）。
#[derive(Clone)]
pub struct ToolRegistry {
    probe: Option<Arc<dyn ControlProbe>>,
    actuator: Option<Arc<dyn Actuator>>,
    redactor: Option<Redactor>,
}

impl ToolRegistry {
    /// 探测可用时的注册表（无操作类——授权门默认关闭）。
    pub fn new(probe: Arc<dyn ControlProbe>) -> Self {
        Self { probe: Some(probe), actuator: None, redactor: None }
    }

    /// 探测不可用（非 Windows / UIA 初始化失败）的降级注册表。
    pub fn without_probe() -> Self {
        Self { probe: None, actuator: None, redactor: None }
    }

    /// 启用操作类工具面（bin 侧 `--actuation` / `HARNESS_ACTUATION=1` 时
    /// 传入平台执行器；非 Windows 传 None 则保持未授权）。
    pub fn with_actuation(mut self, actuator: Option<Arc<dyn Actuator>>) -> Self {
        self.actuator = actuator;
        self
    }

    /// 注入出参脱敏钩子（P2-6 收口；serve/exec `--redact` 接线点）：设置后
    /// **全部感知 op** 的自由文本出参过钩子（capture_scene / get_value /
    /// list_windows / get_focus / dump_tree），密码占位 `<password>` 已在
    /// 感知层遮蔽、不受影响；未设置 = 恒等（缺省，出参明文——消费者按
    /// §22.7 兜底，见模块级契约警告）。
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// 操作类工具面是否已授权（bin 装配提示用）。
    pub fn actuation_enabled(&self) -> bool {
        self.actuator.is_some()
    }

    /// 注册表工具名清单（感知类 + 已授权的操作类；协议层 ping/version
    /// 另由 serve 提供）。
    pub fn tool_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = READ_ONLY_TOOLS.iter().map(|t| t.name).collect();
        if self.actuator.is_some() {
            names.extend(ACTUATION_TOOLS.iter().map(|t| t.name));
        }
        names
    }

    fn probe(&self) -> Result<&Arc<dyn ControlProbe>, Error> {
        self.probe.as_ref().ok_or(Error::ProbeUnavailable)
    }

    fn actuator(&self) -> Result<&Arc<dyn Actuator>, Error> {
        self.actuator.as_ref().ok_or(Error::ActuationLocked)
    }

    /// 执行一次工具调用（同步、可能阻塞——调用方负责放阻塞线程/超时）。
    /// 参数非法 / 工具未知 / 探测不可用 / 操作面未授权均返回 Err（Display
    /// 文案直出 serve 错误响应，P3-13 枚举化后与原字符串逐字节等价）。
    pub fn call(&self, op: &str, args: &Value) -> Result<Value, Error> {
        match op {
            "ping" => Ok(json!({ "pong": true, "ts": now_ms() })),
            // version（upgrade.md §7.3 加法式扩展）：新增 platform（编译
            // 目标 OS）与 Linux 侧的 backends 能力矩阵（detect_backends
            // 序列化）；非 Linux 省略 backends 键（缺省即无该面，旧客户端
            // 加法式不受影响）。
            "version" => {
                #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
                let mut value = json!({
                    "name": "harness",
                    "version": crate::harness::CRATE_VERSION,
                    "protocol": crate::harness::PROTOCOL_VERSION,
                    "snapshotAvailable": self.probe.is_some(),
                    "actuation": self.actuator.is_some(),
                    "tools": self.tool_names(),
                    "platform": std::env::consts::OS,
                });
                #[cfg(target_os = "linux")]
                if let Ok(backends) =
                    serde_json::to_value(crate::harness::snapshot::linux::detect_backends())
                {
                    value["backends"] = backends;
                }
                Ok(value)
            }
            "list_windows" => {
                let mut windows = self.probe()?.list_windows();
                // P2-6 收口（R3）：窗口标题是自由文本，注入 redactor 时
                // 逐条过钩子（handle/processName 结构字段不动）。
                if let Some(redact) = &self.redactor {
                    for window in &mut windows {
                        window.title = redact(&window.title);
                    }
                }
                Ok(json!({ "windows": windows }))
            }
            "get_focus" => {
                let obs = self.probe()?.focused().ok_or(Error::NoFocusedControl)?;
                // P2-6 收口（R3）：control/label 两个 ControlRef 的自由文本
                // （name/windowTitle）过钩子（与 focus 事件载荷同一字段集）。
                let control = redact_control_ref(&obs.control, self.redactor.as_ref());
                let label = obs
                    .label
                    .as_ref()
                    .map(|label| redact_control_ref(label, self.redactor.as_ref()));
                Ok(observation_json_parts(&control, label))
            }
            "get_control" => {
                let (x, y) = xy_of(args)?;
                match self.probe()?.control_at(x, y) {
                    Some(obs) => Ok(observation_json(&obs)),
                    None => Err(Error::NoControlAtPoint),
                }
            }
            "get_value" => {
                let (x, y) = xy_of(args)?;
                let probe = self.probe()?;
                let value = probe.value_at(x, y).ok_or(Error::NoValueAtPoint)?;
                // 出参脱敏（P2-6）：ValuePattern 读值与 Name 兜底路径在此
                // 统一过钩子（兜底发生在探测内部，最终读值均经此处离开）。
                let value = match &self.redactor {
                    Some(redact) => redact(&value),
                    None => value,
                };
                Ok(json!({ "value": value }))
            }
            "dump_tree" => {
                let (depth, nodes) = tree_limits_of(args);
                let mut tree = self.probe()?.dump_tree(depth, nodes);
                // P2-6 收口（R3）：树节点 name 是自由文本，注入 redactor 时
                // 逐条过钩子（path/controlType 等结构字段不动）。
                if let Some(redact) = &self.redactor {
                    for node in &mut tree {
                        node.name = redact(&node.name);
                    }
                }
                Ok(json!({ "nodes": tree }))
            }
            "capture_scene" => {
                let opts = scene_options_of(args)?;
                // 脱敏钩子（P2-6）：注册表注入的 redactor 直接透传给
                // capture_scene（复用 store::Redactor 落盘同款钩子类型）；
                // None = 恒等（未注入时 stdout 明文出参，§22.7 兜底）。
                crate::harness::snapshot::capture_scene(
                    self.probe()?.as_ref(),
                    opts,
                    self.redactor.as_ref(),
                )
                .ok_or(Error::NoForegroundWindow)
                .and_then(|scene| serde_json::to_value(scene).map_err(Error::SceneSerialize))
            }
            "capture_screen" => {
                // 只读抓屏不依赖 UIA 探测（GDI 独立可用，探测不可用时同样
                // 工作；非 Windows 平台 op 内 Err → serve 层转 ok:false）。
                // 区域参数（P3-12）：整屏 base64 单行可达数十 MB——调用方
                // 可只抓目标矩形（参数校验在 screen_region_of，错误直出）。
                let shot = match screen_region_of(args)? {
                    None => crate::harness::snapshot::screen::capture_screen()?,
                    Some((x, y, width, height)) => {
                        crate::harness::snapshot::screen::capture_screen_region(x, y, width, height)?
                    }
                };
                Ok(json!({
                    "pngBase64": crate::harness::snapshot::screen::base64_encode(&shot.png),
                    "width": shot.width,
                    "height": shot.height,
                }))
            }
            "click" => {
                let actuator = self.actuator()?;
                let (x, y, button, clicks) = click_args(args)?;
                actuator.click(x, y, button, clicks)?;
                Ok(json!({ "clicked": { "x": x, "y": y }, "button": button_name(button), "clicks": clicks }))
            }
            "drag" => {
                let actuator = self.actuator()?;
                let drag = drag_args(args)?;
                actuator.drag(
                    drag.from.0,
                    drag.from.1,
                    drag.to.0,
                    drag.to.1,
                    &drag.via,
                    drag.duration_ms,
                    drag.steps,
                )?;
                Ok(json!({
                    "dragged": { "from": drag.from, "to": drag.to, "viaPoints": drag.via.len() }
                }))
            }
            "type_text" => {
                let actuator = self.actuator()?;
                let (text, interval_ms) = type_text_args(args)?;
                actuator.type_text(&text, interval_ms)?;
                Ok(json!({ "typed": text.chars().count() }))
            }
            "key" => {
                let actuator = self.actuator()?;
                let combo = args
                    .get("combo")
                    .and_then(Value::as_str)
                    .ok_or(Error::ComboMissing)?;
                // 参数即校验：注册表层先解析（错误直出），执行器再消费。
                crate::harness::actuate::parse_combo(combo)?;
                actuator.key_combo(combo)?;
                Ok(json!({ "keyed": combo }))
            }
            "set_value" => {
                let actuator = self.actuator()?;
                let ((x, y), value) = set_value_args(args)?;
                actuator.set_value_at(x, y, &value)?;
                Ok(json!({ "set": { "x": x, "y": y } }))
            }
            other => Err(Error::UnknownOp(other.to_string())),
        }
    }
}

fn observation_json(obs: &crate::harness::snapshot::ControlObservation) -> Value {
    json!({
        "control": obs.control,
        "label": obs.label,
    })
}

/// [`observation_json`] 的已脱敏部件形态（get_focus 用：control/label 在
/// 调用方先过 [`redact_control_ref`]，此处只组装——保持与未脱敏形态同一
/// 序列化键集）。
fn observation_json_parts(control: &crate::harness::store::ControlRef, label: Option<crate::harness::store::ControlRef>) -> Value {
    json!({
        "control": control,
        "label": label,
    })
}

/// get_control 参数解析（屏幕坐标）。
fn xy_of(args: &Value) -> Result<(f64, f64), Error> {
    let x = args.get("x").and_then(Value::as_f64);
    let y = args.get("y").and_then(Value::as_f64);
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(Error::MissingXY),
    }
}

/// dump_tree 参数解析（缺省 + 硬上限钳制）。
fn tree_limits_of(args: &Value) -> (u32, usize) {
    let depth = args
        .get("maxDepth")
        .and_then(Value::as_u64)
        .map(|v| v.min(DUMP_TREE_MAX_DEPTH_CAP as u64) as u32)
        .unwrap_or(DUMP_TREE_DEFAULT_DEPTH)
        .max(1);
    let nodes = args
        .get("maxNodes")
        .and_then(Value::as_u64)
        .map(|v| v.min(DUMP_TREE_MAX_NODES_CAP as u64) as usize)
        .unwrap_or(DUMP_TREE_MAX_NODES_DEFAULT)
        .max(1);
    (depth, nodes)
}

/// capture_scene 参数解析（x/y 可选目标坐标 + 树上限复用 [`tree_limits_of`]）。
fn scene_options_of(args: &Value) -> Result<SceneCaptureOptions, Error> {
    let target = match (
        args.get("x").and_then(Value::as_f64),
        args.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => Some((x, y)),
        (None, None) => None,
        _ => return Err(Error::SceneXYPartial),
    };
    let (max_depth, max_nodes) = tree_limits_of(args);
    Ok(SceneCaptureOptions { target, max_depth, max_nodes })
}

/// capture_screen 区域参数解析（P3-12）：`x`/`y`/`width`/`height` 四参
/// 要么全省略（= 整虚拟屏，原行为）要么全给出（= 区域截图）；缺任一 /
/// 值非整数报错。`width`/`height` 限 `1..=`[`CAPTURE_SCREEN_MAX_DIMENSION`]；
/// 坐标不设限（越界由捕获层按虚拟屏边界钳制，空交报错）。
fn screen_region_of(args: &Value) -> Result<Option<(i32, i32, i32, i32)>, Error> {
    // key 在场但值非整数 → 明确报错（与「缺参」区分）；整数 JSON 形态
    // （含负数）之外一律拒绝（浮点/字符串/布尔等）。
    let int_field = |key: &str| -> Result<Option<i64>, Error> {
        match args.get(key) {
            None => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or(Error::ScreenFieldNotInt {
                field: key.to_string(),
                value: value.clone(),
            }),
        }
    };
    let (x, y, width, height) = (
        int_field("x")?,
        int_field("y")?,
        int_field("width")?,
        int_field("height")?,
    );
    match (x, y, width, height) {
        (None, None, None, None) => Ok(None),
        (Some(x), Some(y), Some(width), Some(height)) => {
            for (key, value) in [("width", width), ("height", height)] {
                if !(1..=i64::from(CAPTURE_SCREEN_MAX_DIMENSION)).contains(&value) {
                    return Err(Error::ScreenDimOutOfRange {
                        field: key.to_string(),
                        value,
                        cap: CAPTURE_SCREEN_MAX_DIMENSION,
                    });
                }
            }
            // 坐标须落在 i32（虚拟屏坐标系本身的宽度）——超出无意义，报错
            // 优于静默截断。
            let x = i32::try_from(x)
                .map_err(|_| Error::ScreenCoordOutOfRange { axis: "x", value: x })?;
            let y = i32::try_from(y)
                .map_err(|_| Error::ScreenCoordOutOfRange { axis: "y", value: y })?;
            Ok(Some((x, y, width as i32, height as i32)))
        }
        _ => Err(Error::ScreenArgsPartial),
    }
}

/* --- 操作类 op 参数解析（v1.11 Q；错误文案直出 serve 响应） ------------------ */

/// click 参数：`{x,y,button?,clicks?}`（缺省左键单击；clicks 钳制 1..=3）。
fn click_args(args: &Value) -> Result<(f64, f64, MouseButton, u32), Error> {
    let (x, y) = xy_of(args)?;
    let button = match args.get("button").and_then(Value::as_str) {
        None | Some("left") => MouseButton::Left,
        Some("right") => MouseButton::Right,
        Some(other) => return Err(Error::UnknownMouseButton(other.to_string())),
    };
    let clicks = args.get("clicks").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3) as u32;
    Ok((x, y, button, clicks))
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
    }
}

/// drag 参数：`{fromX,fromY,toX,toY,via?,durationMs?,steps?}`（缺省
/// 250ms/16 步；duration 钳制 0..=5000，steps 钳制 2..=200；`via` 为可选
/// 途经点 `[[x,y],...]` 二元数值数组——与 Python 侧 `ctx.ui.drag(via=…)`
/// 归一化点序列对接，执行为「按下 → 依次经过各途经点 → 终点 → 抬起」，
/// 空/缺省 = 直线拖拽）。
struct DragArgs {
    from: (f64, f64),
    to: (f64, f64),
    /// 可选途经点（协议 `via`；按下后依次平滑经过，再到终点抬起）。
    via: Vec<(f64, f64)>,
    duration_ms: u64,
    steps: u32,
}

fn drag_args(args: &Value) -> Result<DragArgs, Error> {
    let from = match (
        args.get("fromX").and_then(Value::as_f64),
        args.get("fromY").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(Error::DragFromMissing),
    };
    let to = match (
        args.get("toX").and_then(Value::as_f64),
        args.get("toY").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(Error::DragToMissing),
    };
    let via = via_points_of(args)?;
    let duration_ms = args.get("durationMs").and_then(Value::as_u64).unwrap_or(250).min(5000);
    let steps = args.get("steps").and_then(Value::as_u64).unwrap_or(16).clamp(2, 200) as u32;
    Ok(DragArgs { from, to, via, duration_ms, steps })
}

/// 解析 drag 的 `via` 途经点（`[[x,y],...]` 二元数值数组；元素不接受
/// `{"x":..,"y":..}` 对象形态——协议单一形态防歧义，Python 侧按此对接）。
fn via_points_of(args: &Value) -> Result<Vec<(f64, f64)>, Error> {
    let Some(list) = args.get("via") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or(Error::ViaNotArray)?;
    list.iter()
        .enumerate()
        .map(|(i, point)| {
            let pair = point.as_array().filter(|p| p.len() == 2).ok_or(Error::ViaPointInvalid { index: i })?;
            match (pair[0].as_f64(), pair[1].as_f64()) {
                (Some(x), Some(y)) => Ok((x, y)),
                _ => Err(Error::ViaPointInvalid { index: i }),
            }
        })
        .collect()
}

/// type_text 参数：`{text,intervalMs?}`（缺省 3ms 间隔；钳制 0..=100）。
fn type_text_args(args: &Value) -> Result<(String, u64), Error> {
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or(Error::TypeTextMissing)?;
    if text.is_empty() {
        return Err(Error::TypeTextEmpty);
    }
    let interval_ms = args.get("intervalMs").and_then(Value::as_u64).unwrap_or(3).min(100);
    Ok((text.to_string(), interval_ms))
}

/// set_value 参数：`{value,x,y}`（v1 定位只支持坐标；value 可为空串）。
fn set_value_args(args: &Value) -> Result<((f64, f64), String), Error> {
    let (x, y) = xy_of(args)?;
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or(Error::SetValueMissing)?;
    Ok(((x, y), value.to_string()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::snapshot::synth::{SyntheticProbe, SynthNode};
    use crate::harness::snapshot::testutil::{control, rect, sample_tree, sample_windows};
    use crate::harness::snapshot::{ControlObservation, WindowRef};
    use crate::harness::store::SceneNode;

    fn registry() -> ToolRegistry {
        ToolRegistry::new(Arc::new(SyntheticProbe::new(sample_tree(), sample_windows())))
    }

    /// 记录器执行器（合成测试底座：按序记录原语调用）。
    #[derive(Default)]
    struct RecordingActuator(std::sync::Mutex<Vec<String>>);

    impl RecordingActuator {
        fn calls(&self) -> Vec<String> {
            self.0.lock().expect("recorder mutex").clone()
        }
    }

    impl Actuator for RecordingActuator {
        fn click(&self, x: f64, y: f64, button: MouseButton, clicks: u32) -> crate::harness::Result<()> {
            self.0.lock().expect("recorder mutex").push(format!("click {x},{y} {button:?} x{clicks}"));
            Ok(())
        }
        fn drag(
            &self,
            fx: f64,
            fy: f64,
            tx: f64,
            ty: f64,
            via: &[(f64, f64)],
            duration_ms: u64,
            steps: u32,
        ) -> crate::harness::Result<()> {
            self.0
                .lock()
                .expect("recorder mutex")
                .push(format!("drag {fx},{fy}->via{via:?}->{tx},{ty} {duration_ms}ms/{steps}"));
            Ok(())
        }
        fn type_text(&self, text: &str, interval_ms: u64) -> crate::harness::Result<()> {
            self.0.lock().expect("recorder mutex").push(format!("type {text:?}/{interval_ms}"));
            Ok(())
        }
        fn key_combo(&self, combo: &str) -> crate::harness::Result<()> {
            self.0.lock().expect("recorder mutex").push(format!("key {combo}"));
            Ok(())
        }
        fn set_value_at(&self, x: f64, y: f64, text: &str) -> crate::harness::Result<()> {
            self.0.lock().expect("recorder mutex").push(format!("set {x},{y}={text:?}"));
            Ok(())
        }
    }

    fn actuated_registry() -> ToolRegistry {
        registry().with_actuation(Some(Arc::new(RecordingActuator::default())))
    }

    #[test]
    fn sensing_only_surface_without_actuation_authorization() {
        // 未授权进程（无 actuator）：感知七件照常；操作类一律授权门错误
        // （v1.11 Q：方案乙取代原「永不注册」断言——授权而非移除，§22.7）。
        let reg = registry();
        assert_eq!(
            reg.tool_names(),
            vec![
                "list_windows",
                "get_focus",
                "get_control",
                "get_value",
                "dump_tree",
                "capture_scene",
                "capture_screen"
            ],
            "未授权注册表仅感知七件"
        );
        for op in ["click", "set_value", "key", "drag", "type_text"] {
            let err = reg.call(op, &json!({})).unwrap_err();
            assert!(err.to_string().contains(ACTUATION_LOCKED_MESSAGE), "操作类 {op} 未授权错误: {err}");
        }
        assert!(!reg.actuation_enabled());
        let version = reg.call("version", &json!({})).unwrap();
        assert_eq!(version["actuation"], json!(false));
    }

    #[test]
    fn actuation_surface_routes_to_recorder_with_arg_parsing() {
        let recorder = Arc::new(RecordingActuator::default());
        let reg = registry().with_actuation(Some(recorder.clone() as Arc<dyn Actuator>));
        assert_eq!(
            reg.tool_names().len(),
            12,
            "授权后感知 7 + 操作 5: {:?}",
            reg.tool_names()
        );
        assert!(reg.actuation_enabled());
        let version = reg.call("version", &json!({})).unwrap();
        assert_eq!(version["actuation"], json!(true));

        // click：缺省左键单击 / 显式右键双击；clicks 钳制。
        reg.call("click", &json!({ "x": 10.0, "y": 20.0 })).unwrap();
        reg.call("click", &json!({ "x": 1.0, "y": 2.0, "button": "right", "clicks": 9 })).unwrap();
        assert!(reg.call("click", &json!({ "x": 1.0, "button": "right" })).is_err(), "缺 y");
        assert!(reg.call("click", &json!({ "x": 1.0, "y": 2.0, "button": "mid" })).is_err(), "未知按键");

        // drag：起点/终点成对；缺省时长与步数；via 缺省 = 直线。
        reg.call("drag", &json!({ "fromX": 0.0, "fromY": 0.0, "toX": 50.0, "toY": 60.0 })).unwrap();
        assert!(reg.call("drag", &json!({ "fromX": 1.0 })).is_err(), "拖拽起点缺 fromY");
        // via 途经点下发（[[x,y],...] 二元数组 → Actuator::drag 的 via 序列）。
        reg.call(
            "drag",
            &json!({ "fromX": 0.0, "fromY": 0.0, "toX": 30.0, "toY": 0.0, "via": [[10.0, 20.0], [20.0, 20.0]] }),
        )
        .unwrap();
        // via 非法形态：非数组 / 元素非二元 / 坐标非数值。
        assert!(reg.call("drag", &json!({ "fromX": 0.0, "fromY": 0.0, "toX": 1.0, "toY": 1.0, "via": 3 })).is_err(), "via 非数组");
        assert!(
            reg.call("drag", &json!({ "fromX": 0.0, "fromY": 0.0, "toX": 1.0, "toY": 1.0, "via": [[1.0, 2.0, 3.0]] })).is_err(),
            "via 元素须二元"
        );
        assert!(
            reg.call("drag", &json!({ "fromX": 0.0, "fromY": 0.0, "toX": 1.0, "toY": 1.0, "via": [[1.0, "a"]] })).is_err(),
            "via 坐标须数值"
        );

        // type_text / key / set_value。
        reg.call("type_text", &json!({ "text": "你好 wr" })).unwrap();
        assert!(reg.call("type_text", &json!({ "text": "" })).is_err(), "空文本");
        reg.call("key", &json!({ "combo": "ctrl+s" })).unwrap();
        assert!(reg.call("key", &json!({ "combo": "ctrl+alt" })).is_err(), "主键为修饰键");
        assert!(reg.call("key", &json!({})).is_err(), "缺 combo");
        reg.call("set_value", &json!({ "x": 5.0, "y": 6.0, "value": "42" })).unwrap();
        assert!(reg.call("set_value", &json!({ "x": 5.0, "y": 6.0 })).is_err(), "缺 value");

        // 记录器回执：成功的调用按序落到执行器（调用序列契约，供回放调试）。
        assert_eq!(
            recorder.calls(),
            vec![
                "click 10,20 Left x1".to_string(),
                "click 1,2 Right x3".to_string(),
                "drag 0,0->via[]->50,60 250ms/16".to_string(),
                "drag 0,0->via[(10.0, 20.0), (20.0, 20.0)]->30,0 250ms/16".to_string(),
                "type \"你好 wr\"/3".to_string(),
                "key ctrl+s".to_string(),
                "set 5,6=\"42\"".to_string(),
            ]
        );
    }

    #[test]
    fn ping_and_version_do_not_need_probe() {
        let reg = ToolRegistry::without_probe();
        let pong = reg.call("ping", &json!({})).unwrap();
        assert_eq!(pong["pong"], json!(true));
        let version = reg.call("version", &json!({})).unwrap();
        assert_eq!(version["name"], json!("harness"));
        assert_eq!(version["protocol"], json!(crate::harness::PROTOCOL_VERSION));
        assert_eq!(version["snapshotAvailable"], json!(false));
    }

    #[test]
    fn sensing_ops_route_to_probe() {
        let reg = registry();
        let windows = reg.call("list_windows", &json!({})).unwrap();
        assert_eq!(windows["windows"][0]["title"], json!("报销单 - 记事本"));

        let focus = reg.call("get_focus", &json!({})).unwrap();
        assert_eq!(focus["control"]["controlType"], json!("Edit"));

        let hit = reg.call("get_control", &json!({ "x": 15.0, "y": 15.0 })).unwrap();
        assert_eq!(hit["control"]["name"], json!("保存"));
        assert!(reg.call("get_control", &json!({ "x": 1.0 })).is_err(), "缺 y 参数");
        assert!(reg.call("get_control", &json!({ "x": 900.0, "y": 700.0 })).is_err(), "未命中");

        let tree = reg.call("dump_tree", &json!({ "maxDepth": 1 })).unwrap();
        assert_eq!(tree["nodes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sensing_ops_fail_closed_without_probe() {
        let reg = ToolRegistry::without_probe();
        let err = reg.call("list_windows", &json!({})).unwrap_err();
        assert!(err.to_string().contains("探测不可用"), "{err}");
        assert!(reg.call("capture_scene", &json!({})).unwrap_err().to_string().contains("探测不可用"));
    }

    #[test]
    fn capture_scene_op_assembles_scene_and_parses_args() {
        let reg = registry();
        // 缺省参数：targetControl = 焦点控件（Edit），树全量，camelCase 契约键。
        let scene = reg.call("capture_scene", &json!({})).unwrap();
        assert_eq!(scene["window"]["controlType"], json!("Window"));
        assert_eq!(scene["window"]["name"], json!("报销单 - 记事本"));
        assert_eq!(scene["targetControl"]["controlType"], json!("Edit"));
        assert_eq!(scene["treePruned"].as_array().unwrap().len(), 5);
        assert_eq!(scene["screenshots"], json!([]));
        // x/y 目标坐标：命中保存按钮。
        let scene = reg
            .call("capture_scene", &json!({ "x": 15.0, "y": 15.0 }))
            .unwrap();
        assert_eq!(scene["targetControl"]["name"], json!("保存"));
        // maxDepth/maxNodes 钳制进 SceneCaptureOptions（树截断）。
        let scene = reg
            .call("capture_scene", &json!({ "maxDepth": 1 }))
            .unwrap();
        assert_eq!(scene["treePruned"].as_array().unwrap().len(), 1);
        // x/y 不成对 → 参数错误；未命中坐标 → targetControl 缺席（不报错）。
        assert!(reg.call("capture_scene", &json!({ "x": 1.0 })).is_err());
        let scene = reg
            .call("capture_scene", &json!({ "x": 900.0, "y": 700.0 }))
            .unwrap();
        assert!(scene.get("targetControl").is_none());
    }

    /// 注入 redactor 后 capture_scene 出参自由文本被替换（P2-6 工具侧）：
    /// 窗口标题/控件 name/树节点 name 过钩子，结构字段与未注入形态不变。
    #[test]
    fn injected_redactor_redacts_capture_scene_free_text() {
        let redact: Redactor = Arc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let tree = SynthNode::branch(
            control("Window", "主窗口 sk-live-abc", rect(0.0, 0.0, 800.0, 600.0)),
            vec![SynthNode::focused_leaf(control("Edit", "内容 sk-live-abc", rect(0.0, 0.0, 100.0, 24.0)))],
        );
        let windows =
            vec![WindowRef { handle: 42, title: "主窗口 sk-live-abc".into(), process_name: "notepad.exe".into() }];
        let reg = ToolRegistry::new(Arc::new(SyntheticProbe::new(tree.clone(), windows.clone())))
            .with_redactor(redact);
        let scene = reg.call("capture_scene", &json!({})).unwrap();
        assert_eq!(scene["window"]["name"], json!("主窗口 <redacted>"), "窗口 name 脱敏");
        assert_eq!(scene["window"]["windowTitle"], json!("主窗口 <redacted>"));
        assert_eq!(scene["targetControl"]["name"], json!("内容 <redacted>"), "焦点控件 name 脱敏");
        assert_eq!(scene["treePruned"][0]["name"], json!("主窗口 <redacted>"), "树节点 name 脱敏");
        assert_eq!(scene["window"]["processName"], json!("notepad.exe"), "结构字段不脱敏");
        assert_eq!(scene["window"]["handle"], json!(42));
        // 未注入（缺省）= 恒等：明文原样出参（契约警告的对照）。
        let plain = ToolRegistry::new(Arc::new(SyntheticProbe::new(tree, windows)));
        let scene = plain.call("capture_scene", &json!({})).unwrap();
        assert_eq!(scene["window"]["name"], json!("主窗口 sk-live-abc"));
    }

    /// 注入 redactor 后 get_value 读值过钩子（P2-6）；密码占位
    /// `<password>` 在感知层已遮蔽，redactor 对其无动作；未注入 = 恒等。
    /// （SyntheticProbe 缺省不支持取值，以 ValueProbe 承载读值路径。）
    struct ValueProbe(&'static str);

    impl ControlProbe for ValueProbe {
        fn control_at(&self, _x: f64, _y: f64) -> Option<ControlObservation> {
            None
        }
        fn focused(&self) -> Option<ControlObservation> {
            None
        }
        fn foreground_window(&self) -> Option<WindowRef> {
            None
        }
        fn list_windows(&self) -> Vec<WindowRef> {
            Vec::new()
        }
        fn dump_tree(&self, _max_depth: u32, _max_nodes: usize) -> Vec<SceneNode> {
            Vec::new()
        }
        fn value_at(&self, _x: f64, _y: f64) -> Option<String> {
            Some(self.0.into())
        }
    }

    #[test]
    fn injected_redactor_redacts_get_value_but_not_password_placeholder() {
        let redact: Redactor = Arc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let reg = ToolRegistry::new(Arc::new(ValueProbe("余额 sk-live-abc"))).with_redactor(redact);
        let out = reg.call("get_value", &json!({ "x": 1.0, "y": 1.0 })).unwrap();
        assert_eq!(out["value"], json!("余额 <redacted>"), "读值脱敏（含 Name 兜底路径）");
        // 密码占位不受影响：感知层已遮蔽的 <password> 原样通过。
        let redact: Redactor = Arc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let reg =
            ToolRegistry::new(Arc::new(ValueProbe(crate::harness::snapshot::PASSWORD_PLACEHOLDER)))
                .with_redactor(redact);
        let out = reg.call("get_value", &json!({ "x": 1.0, "y": 1.0 })).unwrap();
        assert_eq!(out["value"], json!("<password>"), "密码占位不被二次改写");
        // 未注入（缺省）= 恒等。
        let reg = ToolRegistry::new(Arc::new(ValueProbe("余额 sk-live-abc")));
        let out = reg.call("get_value", &json!({ "x": 1.0, "y": 1.0 })).unwrap();
        assert_eq!(out["value"], json!("余额 sk-live-abc"));
    }

    /// P2-6 收口（R3）：注入 redactor 后 list_windows / get_focus /
    /// dump_tree 的自由文本同样过钩子——感知 op 出参不再有明文旁路。
    #[test]
    fn injected_redactor_redacts_list_windows_get_focus_and_dump_tree() {
        let redact: Redactor = Arc::new(|s: &str| s.replace("sk-live-abc", "<redacted>"));
        let tree = SynthNode::branch(
            control("Window", "主窗口 sk-live-abc", rect(0.0, 0.0, 800.0, 600.0)),
            vec![SynthNode::focused_leaf(control("Edit", "备注 sk-live-abc", rect(0.0, 0.0, 300.0, 24.0)))],
        );
        let windows = vec![
            WindowRef { handle: 42, title: "主窗口 sk-live-abc".into(), process_name: "notepad.exe".into() },
            WindowRef { handle: 7, title: "纯结构对照窗口".into(), process_name: "workrecorder.exe".into() },
        ];
        let reg = ToolRegistry::new(Arc::new(SyntheticProbe::new(tree.clone(), windows.clone())))
            .with_redactor(redact);

        // list_windows：标题过钩子，handle/processName 不动。
        let out = reg.call("list_windows", &json!({})).unwrap();
        assert_eq!(out["windows"][0]["title"], json!("主窗口 <redacted>"), "窗口标题脱敏");
        assert_eq!(out["windows"][0]["handle"], json!(42), "结构字段不脱敏");
        assert_eq!(out["windows"][0]["processName"], json!("notepad.exe"));
        assert_eq!(out["windows"][1]["title"], json!("纯结构对照窗口"), "未命中原样");

        // get_focus：control 与 label 的自由文本（name/windowTitle）过钩子
        //（testutil.control 的 windowTitle 固定样本值，未命中规则原样通过）。
        let out = reg.call("get_focus", &json!({})).unwrap();
        assert_eq!(out["control"]["name"], json!("备注 <redacted>"), "焦点控件 name 脱敏");
        assert_eq!(out["control"]["windowTitle"], json!("报销单 - 记事本"), "未命中文本原样");

        // dump_tree：各节点 name 过钩子（树 = 根 Window + focused Edit 两节点）。
        let out = reg.call("dump_tree", &json!({})).unwrap();
        assert_eq!(out["nodes"][0]["name"], json!("主窗口 <redacted>"), "树根 name 脱敏");
        assert_eq!(out["nodes"][1]["name"], json!("备注 <redacted>"), "树叶子 name 脱敏");
        assert_eq!(out["nodes"][0]["path"], json!("0"), "树路径结构字段不脱敏");

        // 未注入（缺省）= 恒等：明文原样出参（契约警告的对照）。
        let plain = ToolRegistry::new(Arc::new(SyntheticProbe::new(tree, windows)));
        let out = plain.call("list_windows", &json!({})).unwrap();
        assert_eq!(out["windows"][0]["title"], json!("主窗口 sk-live-abc"));
        let out = plain.call("dump_tree", &json!({})).unwrap();
        assert_eq!(out["nodes"][0]["name"], json!("主窗口 sk-live-abc"));
    }

    #[test]
    fn unknown_op_error_mentions_both_surfaces_and_authorization() {
        let err = actuated_registry().call("nope9", &json!({})).unwrap_err();
        let err = err.to_string();
        assert!(err.contains("unknown op: nope9"), "{err}");
        // 未知 op 文案列出全部感知 op（含 v1.13 capture_screen）。
        for op in READ_ONLY_TOOLS.iter().map(|t| t.name) {
            assert!(err.contains(op), "unknown-op 文案缺 {op}: {err}");
        }
        // 已授权注册表上操作类不再是 unknown，而是真实路由——确认 type_text
        // 不在 unknown 之列（授权门语义，区别于未知 op）。
        let reg = actuated_registry();
        assert!(reg.call("type_text", &json!({ "text": "x" })).is_ok());
    }

    /// capture_screen 只读注册（v1.13）：清单含它、无参调用形态存在；
    /// 真实抓屏不在单测执行（需交互桌面）——Windows 冒烟见
    /// `tests/screen_smoke.rs`，非 Windows 的 ok:false 降级见
    /// `snapshot/screen.rs` 合成单测。
    #[test]
    fn capture_screen_registered_as_read_only_sensing_tool() {
        assert!(
            READ_ONLY_TOOLS.iter().any(|t| t.name == "capture_screen"),
            "capture_screen 应在只读感知清单: {:?}",
            READ_ONLY_TOOLS.iter().map(|t| t.name).collect::<Vec<_>>()
        );
        // 非 Windows：无参调用直接 ok:false（降级路径合成测试）。
        // Windows 单测不做真实抓屏（需交互桌面，走 #[ignore] 冒烟）。
        #[cfg(not(windows))]
        {
            let reg = ToolRegistry::without_probe();
            // Linux 已接入 X11 截屏（upgrade.md §7.1 P1）：DISPLAY 在场 →
            // 真实成功路径（x11_screen.rs 锁定），降级断言仅适用无显示服务
            // 环境（headless CI）。
            #[cfg(target_os = "linux")]
            if std::env::var("DISPLAY").is_ok_and(|value| !value.trim().is_empty()) {
                return;
            }
            assert!(reg.call("capture_screen", &json!({})).is_err(), "无显示服务的非 Windows 恒 ok:false");
        }
    }

    /// capture_screen 区域参数校验（P3-12）：四参全缺/全有之外一律明确
    /// 报错；宽高限 1..=10000；合法参数经纯函数断言（不在单测真实抓屏）。
    #[test]
    fn capture_screen_region_args_validated() {
        // 全缺 = None（整屏，原行为路径）；全有 = Some（负坐标合法——
        // 多显示器虚拟屏可为负）。Error 无 PartialEq，经 unwrap 比较 Ok 载荷。
        assert_eq!(screen_region_of(&json!({})).unwrap(), None);
        assert_eq!(
            screen_region_of(&json!({ "x": -1920, "y": 0, "width": 800, "height": 600 })).unwrap(),
            Some((-1920, 0, 800, 600))
        );
        // 缺任一参：四种形态各自报「四参全给或全省略」。
        for args in [
            json!({ "x": 1 }),
            json!({ "x": 1, "y": 2, "width": 3 }),
            json!({ "y": 2, "width": 3, "height": 4 }),
            json!({ "width": 100 }),
        ] {
            let err = screen_region_of(&args).unwrap_err().to_string();
            assert!(err.contains("四参全给"), "缺参文案（{args}）: {err}");
        }
        // 非整数（字符串 / 浮点 / 布尔 / null）→ 逐 key 明确报「须为整数」。
        for (key, value) in [("x", json!("abc")), ("width", json!(12.5)), ("height", json!(true)), ("y", json!(null))] {
            let err = screen_region_of(&json!({ "x": 0, "y": 0, "width": 10, "height": 10, key: value }))
                .unwrap_err()
                .to_string();
            assert!(err.contains(key) && err.contains("须为整数"), "非整数文案（{key}）: {err}");
        }
        // 宽/高越界（0 / 负 / 超上限）各自报 1..=10000。
        for (key, value) in [("width", 0), ("height", 0), ("width", -5), ("height", -5), ("width", 10_001), ("height", 999_999)] {
            let err = screen_region_of(&json!({ "x": 0, "y": 0, "width": 10, "height": 10, key: value }))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(key) && err.contains("1..=10000"),
                "越界文案（{key}={value}）: {err}"
            );
        }
        // 坐标超 i32 范围：报坐标范围（而非静默截断）。
        let err = screen_region_of(&json!({ "x": 3_000_000_000i64, "y": 0, "width": 10, "height": 10 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("超出坐标范围"), "{err}");
    }

    /// 注册表层错误路径（参数校验先于捕获调用——Windows 单测同样不做
    /// 真实抓屏）；非 Windows 下合法区域参数走 ok:false 降级（同整屏）。
    #[test]
    fn capture_screen_region_errors_surface_via_registry() {
        let reg = ToolRegistry::without_probe();
        let err = reg.call("capture_screen", &json!({ "x": 1 })).unwrap_err().to_string();
        assert!(err.contains("四参全给"), "{err}");
        let err = reg
            .call("capture_screen", &json!({ "x": 0, "y": 0, "width": 0, "height": 10 }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("width") && err.contains("1..=10000"), "{err}");
        #[cfg(target_os = "linux")]
        {
            // Linux 已接入 X11（P1）：无 DISPLAY 环境才走 no-display-server
            // 降级（DISPLAY 在场 → 真实区域截屏，成功路径由 x11_screen.rs
            // 的真实测试锁定——注册表层只管参数校验先行）。
            if !std::env::var("DISPLAY").is_ok_and(|value| !value.trim().is_empty()) {
                let err = reg
                    .call("capture_screen", &json!({ "x": 0, "y": 0, "width": 100, "height": 100 }))
                    .unwrap_err();
                assert_eq!(err.code(), Some("no-display-server"), "合法区域参数走降级: {err}");
            }
        }
    }

    #[test]
    fn tree_limits_clamp_defaults_and_caps() {
        assert_eq!(tree_limits_of(&json!({})), (8, 512));
        assert_eq!(tree_limits_of(&json!({ "maxDepth": 99, "maxNodes": 999_999 })), (16, 10_000));
        assert_eq!(tree_limits_of(&json!({ "maxDepth": 0, "maxNodes": 0 })), (1, 1));
        assert_eq!(tree_limits_of(&json!({ "maxDepth": -3 })), (8, 512), "负数按缺省");
    }

    #[test]
    fn sample_tree_helper_shapes_are_stable() {
        // testutil 样例的基本不变量（graph/serve 测试同源依赖）。
        assert_eq!(control("Button", "x", rect(0.0, 0.0, 1.0, 1.0)).control_type, "Button");
        assert_eq!(sample_windows().len(), 2);
    }
}
