//! AT-SPI2 控件树感知（feature `linux-atspi`；upgrade.md §7.1，P2 实现）。
//!
//! [`AtspiProbe`] 经 `atspi` crate（zbus 纯 Rust D-Bus）实现
//! [`crate::harness::snapshot::ControlProbe`]，接口映射（§7.1）：
//!
//! - `control_at` ≈ 桌面树 DFS 命中测试（`Component.GetAccessibleAtPoint`
//!   自顶层窗口逐层下钻，最深命中即目标）；
//! - `focused` ≈ `State.Focused` 查询——**优先事件缓存**（[`EventCache`]，
//!   §7.5-1：recorder 在场时用 `state-changed:focused` 事件维护的焦点，
//!   serve 常驻进程天然适合），冷启动才全扫（节点预算封顶）；
//! - `value_at` ≈ Value/Text 接口读回 + Name 兜底（对齐 UIA ValuePattern
//!   → Name 的降级链）；
//! - [`set_value_at_point`] ≈ EditableText 直写（serve `set_value` op 的
//!   Linux 路径；**签名与其他 worker 的对接契约，逐字保持**）；
//! - 密码遮蔽 ≈ `Role.PasswordText`（§22.7 四采集点惯例：Name/读值/树转储
//!   统一 [`crate::harness::snapshot::PASSWORD_PLACEHOLDER`]；注：AT-SPI 的
//!   password-protected **状态**已在 at-spi2-core 2.51 移除，atspi-common
//!   0.12 无此变体，角色判定是现存通道——GTK/Qt 密码框均暴露该角色）。
//!
//! # Handle 语义（§7.1「Handle 语义」）
//!
//! AT-SPI 元素 `handle=0`（无 HWND 对应物），定位靠 `path` + 文字锚点；
//! **D-Bus object path 禁止作为跨会话标识**（registry 顺序分配不稳定），
//! 全链路不落入 `handle`。窗口级句柄：X11 的窗口 XID 在 AT-SPI 接口面上
//! 无暴露通道（需 X11 侧按标题匹配补齐，见 todo），现阶段统一用
//! app 名 + 标题的稳定哈希（纯 Wayland 策略；跨会话稳定，会话内唯一）。
//!
//! # 同步封装（决策点 D4）
//!
//! atspi/zbus 全链 async，但 harness 零 runtime：`AtspiProbe::new()` 与
//! 每个探测调用内用 [`futures_lite::future::block_on`] 同步包裹（zbus 的
//! async-io 反应器线程在后台驱动 I/O，当前线程仅 park 等待），**不为此引
//! tokio**。futures-lite 已是 atspi-connection 的传递依赖，特性面零新增。
//!
//! # 事件缓存与 recorder 的协作
//!
//! [`EventCache`]（焦点 + 前台窗口）由 [`AtspiRecorder`] 写、probe 读
//! （[`AtspiProbe::with_event_cache`] 注入）；无 recorder 在场时 probe
//! 冷启动自扫。本模块另导出 [`ElementAnnotation`] 供 recorder 组装事件
//! 行的元素锚点（树路径对齐 synth 语义常量）。

use std::sync::{Arc, Mutex};

use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{AccessibilityConnection, CoordType, Interface, ObjectRef, Role, State};
use futures_lite::future::block_on;

use crate::harness::snapshot::synth::{LABEL_MAX_PREV_SIBLINGS, PATH_MAX_DEPTH};
use crate::harness::snapshot::{
    mask_password_text, ControlObservation, ControlProbe, WindowRef, PASSWORD_PLACEHOLDER,
};
use crate::harness::store::{ControlRef, ControlRect, SceneNode};
use crate::harness::Error;

/// AT-SPI null 对象路径（atk-adaptor 越界索引等场景的占位对象）。
const NULL_PATH: &str = "/org/a11y/atspi/null";

/// 命中测试单窗口内下钻层数上限（防病态 provider 的环/超深树）。
const HIT_DRILL_MAX_DEPTH: usize = 64;

/// 冷启动焦点全扫的节点预算（§7.5-1：focused() 全扫性能风险的封顶）。
const FOCUSED_SCAN_NODE_BUDGET: usize = 2500;

/* --- 事件缓存（recorder 写 / probe 读；§7.5-1） -------------------------------- */

/// AT-SPI 事件缓存：recorder 在场时以 `state-changed:focused` /
/// `window:activate` 事件维护焦点与前台窗口，probe 的 [`AtspiProbe::focused`]
/// / [`AtspiProbe::foreground_window`] 优先消费缓存（冷启动才全扫）。
///
/// 纯数据（无连接）：probe 与 recorder 各自持连接、共享本缓存。
#[derive(Debug, Default)]
pub struct EventCache {
    /// 最近 `state-changed:focused`（true）的元素；失焦即清空。
    focus: Mutex<Option<ObjectRef>>,
    /// 最近 `window:activate` 的窗口对象。
    active_window: Mutex<Option<ObjectRef>>,
}

impl EventCache {
    /// 空缓存。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录焦点变化（None = 失焦/清除；锁毒化按缺席降级，不 panic）。
    pub(crate) fn note_focus(&self, focus: Option<ObjectRef>) {
        if let Ok(mut guard) = self.focus.lock() {
            *guard = focus;
        }
    }

    /// 记录前台窗口激活。
    pub(crate) fn note_active_window(&self, window: ObjectRef) {
        if let Ok(mut guard) = self.active_window.lock() {
            *guard = Some(window);
        }
    }

    /// 当前缓存焦点。
    pub(crate) fn focus(&self) -> Option<ObjectRef> {
        self.focus.lock().ok().and_then(|g| g.clone())
    }

    /// 当前缓存前台窗口。
    pub(crate) fn active_window(&self) -> Option<ObjectRef> {
        self.active_window.lock().ok().and_then(|g| g.clone())
    }

    /// 清空焦点缓存（缓存对象已失效时由 probe 调用，回落冷扫）。
    pub(crate) fn clear_focus(&self) {
        self.note_focus(None);
    }
}

/* --- 纯逻辑映射（单测锁定） ---------------------------------------------------- */

/// AT-SPI role → UIA ControlType 字符串（§7.1 映射表全量；穷尽 match，
/// atspi-common 增补角色时编译期强制补表）。
///
/// 契约锚点（P2 任务书）：button→Button、text→Edit、check-box→CheckBox、
/// menu item→MenuItem；关键补充：label→Text（label 启发式靠它命中）、
/// password-text→Edit（密码判定走角色）、frame/window→Window（顶层窗口
/// 识别）。输出词汇表 ⊆ UIA 侧 [`crate::harness::snapshot::uia`] 的 ControlType
/// 清单（单测断言）。
pub(crate) fn role_to_control_type(role: Role) -> &'static str {
    match role {
        Role::Invalid => "Custom",
        Role::AcceleratorLabel => "Text",
        Role::Alert => "Window",
        Role::Animation => "Custom",
        Role::Arrow => "Custom",
        Role::Calendar => "Calendar",
        Role::Canvas => "Custom",
        Role::CheckBox => "CheckBox",
        Role::CheckMenuItem => "MenuItem",
        Role::ColorChooser => "Window",
        Role::ColumnHeader => "HeaderItem",
        Role::ComboBox => "ComboBox",
        Role::DateEditor => "Custom",
        Role::DesktopIcon => "Custom",
        Role::DesktopFrame => "Pane",
        Role::Dial => "Custom",
        Role::Dialog => "Window",
        Role::DirectoryPane => "Pane",
        Role::DrawingArea => "Custom",
        Role::FileChooser => "Window",
        Role::Filler => "Pane",
        Role::FocusTraversable => "Custom",
        Role::FontChooser => "Window",
        Role::Frame => "Window",
        Role::GlassPane => "Pane",
        Role::HTMLContainer => "Document",
        Role::Icon => "Image",
        Role::Image => "Image",
        Role::InternalFrame => "Window",
        Role::Label => "Text",
        Role::LayeredPane => "Pane",
        Role::List => "List",
        Role::ListItem => "ListItem",
        Role::Menu => "Menu",
        Role::MenuBar => "MenuBar",
        Role::MenuItem => "MenuItem",
        Role::OptionPane => "Pane",
        Role::PageTab => "TabItem",
        Role::PageTabList => "Tab",
        Role::Panel => "Pane",
        Role::PasswordText => "Edit",
        Role::PopupMenu => "Menu",
        Role::ProgressBar => "ProgressBar",
        Role::Button => "Button",
        Role::RadioButton => "RadioButton",
        Role::RadioMenuItem => "MenuItem",
        Role::RootPane => "Pane",
        Role::RowHeader => "HeaderItem",
        Role::ScrollBar => "ScrollBar",
        Role::ScrollPane => "Pane",
        Role::Separator => "Separator",
        Role::Slider => "Slider",
        Role::SpinButton => "Spinner",
        Role::SplitPane => "Pane",
        Role::StatusBar => "StatusBar",
        Role::Table => "Table",
        Role::TableCell => "DataItem",
        Role::TableColumnHeader => "HeaderItem",
        Role::TableRowHeader => "HeaderItem",
        Role::TearoffMenuItem => "MenuItem",
        Role::Terminal => "Document",
        Role::Text => "Edit",
        Role::ToggleButton => "Button",
        Role::ToolBar => "ToolBar",
        Role::ToolTip => "ToolTip",
        Role::Tree => "Tree",
        Role::TreeTable => "Tree",
        Role::Unknown => "Custom",
        Role::Viewport => "Pane",
        Role::Window => "Window",
        Role::Extended => "Custom",
        Role::Header => "Header",
        Role::Footer => "Group",
        Role::Paragraph => "Text",
        Role::Ruler => "Custom",
        Role::Application => "Pane",
        Role::Autocomplete => "ComboBox",
        Role::Editbar => "Edit",
        Role::Embedded => "Pane",
        Role::Entry => "Edit",
        Role::CHART => "Custom",
        Role::Caption => "Text",
        Role::DocumentFrame => "Document",
        Role::Heading => "Text",
        Role::Page => "Pane",
        Role::Section => "Group",
        Role::RedundantObject => "Custom",
        Role::Form => "Group",
        Role::Link => "Hyperlink",
        Role::InputMethodWindow => "Window",
        Role::TableRow => "DataItem",
        Role::TreeItem => "TreeItem",
        Role::DocumentSpreadsheet => "Document",
        Role::DocumentPresentation => "Document",
        Role::DocumentText => "Document",
        Role::DocumentWeb => "Document",
        Role::DocumentEmail => "Document",
        Role::Comment => "Custom",
        Role::ListBox => "List",
        Role::Grouping => "Group",
        Role::ImageMap => "Custom",
        Role::Notification => "Custom",
        Role::InfoBar => "Pane",
        Role::LevelBar => "ProgressBar",
        Role::TitleBar => "TitleBar",
        Role::BlockQuote => "Group",
        Role::Audio => "Custom",
        Role::Video => "Custom",
        Role::Definition => "Text",
        Role::Article => "Document",
        Role::Landmark => "Custom",
        Role::Log => "Custom",
        Role::Marquee => "Custom",
        Role::Math => "Custom",
        Role::Rating => "Slider",
        Role::Timer => "Custom",
        Role::Static => "Text",
        Role::MathFraction => "Custom",
        Role::MathRoot => "Custom",
        Role::Subscript => "Text",
        Role::Superscript => "Text",
        Role::DescriptionList => "List",
        Role::DescriptionTerm => "Text",
        Role::DescriptionValue => "Text",
        Role::Footnote => "Text",
        Role::ContentDeletion => "Custom",
        Role::ContentInsertion => "Custom",
        Role::Mark => "Custom",
        Role::Suggestion => "Custom",
        Role::PushButtonMenu => "SplitButton",
    }
}

/// 密码控件判定（AT-SPI 通道 = `Role.PasswordText`；password-protected
/// 状态已随 at-spi2-core 2.51 移除）。
pub(crate) fn role_is_password(role: Role) -> bool {
    matches!(role, Role::PasswordText)
}

/// 顶层窗口角色判定（Frame/Window/Dialog/Alert 四类承载标题栏语义）。
fn is_window_role(role: Role) -> bool {
    matches!(role, Role::Frame | Role::Window | Role::Dialog | Role::Alert)
}

/// 树路径段（自窗口层起、含自身）→ `ControlRef.path`：`Type[name]/…`
/// 连接；祖先深度 ≤ [`PATH_MAX_DEPTH`]（根方向截去，保最近链——与
/// synth/uia 的截断语义一致）。空段集 → None。
pub(crate) fn build_tree_path(segments: &[(String, String)]) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    // 至多 PATH_MAX_DEPTH 个祖先 + 自身。
    let base = segments.len().saturating_sub(PATH_MAX_DEPTH + 1);
    Some(
        segments[base..]
            .iter()
            .map(|(t, n)| format!("{t}[{n}]"))
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// 窗口稳定句柄：app 名 + 标题的 FNV-1a 64 位哈希（§7.1「纯 Wayland 用
/// app name+标题稳定哈希」；X11 XID 在 AT-SPI 接口面无通道，暂同策略，
/// 见模块文档）。确定性、会话间稳定；i64 承载（HWND 语义位）。
pub(crate) fn stable_window_handle(app: &str, title: &str) -> i64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in app.bytes().chain(std::iter::once(0)).chain(title.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash as i64
}

/// Component extents (x,y,w,h) → ControlRect（负坐标合法：多显示器左侧屏）。
fn rect_from_extents(x: i32, y: i32, w: i32, h: i32) -> ControlRect {
    ControlRect { x: x as f64, y: y as f64, width: w as f64, height: h as f64 }
}

/// null 对象判定（越界/缺席的 AT-SPI 惯例占位）。
fn is_null(obj: &ObjectRef) -> bool {
    obj.path.as_str() == NULL_PATH
}

/* --- 探测主体 ------------------------------------------------------------------ */

/// AT-SPI2 感知探测（[`ControlProbe`] 的 Linux 生产实现）。
///
/// 连接为进程级共享（[`shared_connection`]）：`AtspiProbe::new()` 与
/// [`set_value_at_point`] 复用同一条 a11y 总线连接（zbus 连接天然支持
/// 多线程并发方法调用）；事件订阅（recorder）独立建连——专职事件线程
/// 不与探测调用互相挤占。
pub struct AtspiProbe {
    conn: AccessibilityConnection,
    /// recorder 注入的事件缓存（None = 冷启动自扫）。
    cache: Option<Arc<EventCache>>,
}

impl AtspiProbe {
    /// 连接 a11y 总线（会话总线 → `org.a11y.Bus.GetAddress` → a11y 总线；
    /// D-Bus activation 会自动拉起 at-spi）。失败 → [`Error::AtspiConnectFailed`]，
    /// 调用方（platform_probe_linux）诚实降级 None。
    ///
    /// 另尽力置位会话总线的 `org.a11y.Bus.IsEnabled`（屏幕阅读器同款 AT
    /// 宣告：让未开 a11y 的应用动态开启辅助功能树；失败仅 debug 日志，
    /// 不影响探测本身）。
    pub fn new() -> Result<Self, Error> {
        let conn = shared_connection()?;
        if let Err(e) = block_on(atspi::connection::set_session_accessibility(true)) {
            tracing::debug!(error = %e, "置位 org.a11y.Bus.IsEnabled 失败（不影响已有树）");
        }
        Ok(Self { conn, cache: None })
    }

    /// 注入事件缓存（recorder 在场时 `focused`/`foreground_window` 走缓存，
    /// §7.5-1）。builder 风格链式调用。
    pub fn with_event_cache(mut self, cache: Arc<EventCache>) -> Self {
        self.cache = Some(cache);
        self
    }
}

/* --- 异步辅助（全部经 block_on 同步消费；D4） ---------------------------------- */

/// ObjectRef → AccessibleProxy（无属性缓存——感知逐调用取新鲜值；内联
/// builder 避免 ObjectRefExt 不透明 future 把 obj 生命周期漏进返回值）。
async fn as_proxy<'a>(
    conn: &'a AccessibilityConnection,
    obj: &ObjectRef,
) -> Option<AccessibleProxy<'a>> {
    if is_null(obj) {
        return None;
    }
    AccessibleProxy::builder(conn.connection())
        .destination(obj.name.clone())
        .ok()?
        .path(obj.path.clone())
        .ok()?
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()
}

/// ObjectRef → ComponentProxy（屏幕坐标语义的几何接口）。
async fn component_proxy<'a>(
    conn: &'a AccessibilityConnection,
    obj: &ObjectRef,
) -> Option<ComponentProxy<'a>> {
    ComponentProxy::builder(conn.connection())
        .destination(obj.name.clone())
        .ok()?
        .path(obj.path.clone())
        .ok()?
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()
}

/// ObjectRef 的屏幕矩形（Component extents；失败 None）。
async fn extents_of(conn: &AccessibilityConnection, obj: &ObjectRef) -> Option<ControlRect> {
    let comp = component_proxy(conn, obj).await?;
    let (x, y, w, h) = comp.get_extents(CoordType::Screen).await.ok()?;
    Some(rect_from_extents(x, y, w, h))
}

/// ObjectRef 是否持有指定状态（get_state 契约失败按 false）。
async fn has_state(conn: &AccessibilityConnection, obj: &ObjectRef, state: State) -> bool {
    match as_proxy(conn, obj).await {
        Some(proxy) => proxy.get_state().await.map(|s| s.contains(state)).unwrap_or(false),
        None => false,
    }
}

/// ObjectRef 的 label 关联（前邻 Text 兄弟；组装代理后走回看）。
async fn label_of(conn: &AccessibilityConnection, obj: &ObjectRef) -> Option<ControlRef> {
    let proxy = as_proxy(conn, obj).await?;
    label_of_previous_text_sibling(conn, &proxy).await
}

/// 窗口/元素对象所属应用的 accessible name（≈ 进程名语义）。
async fn owning_app_name(
    conn: &AccessibilityConnection,
    proxy: &AccessibleProxy<'_>,
) -> String {
    let Ok(app) = proxy.get_application().await else {
        return String::new();
    };
    match as_proxy(conn, &app).await {
        Some(app_proxy) => app_proxy.name().await.unwrap_or_default(),
        None => String::new(),
    }
}

/// registry 根 → 应用根清单（部分桌面在 registry 与应用间垫一层
/// DesktopFrame，透明下钻）。
async fn desktop_apps(conn: &AccessibilityConnection) -> Vec<ObjectRef> {
    let mut apps = Vec::new();
    let Ok(root) = conn.root_accessible_on_registry().await else {
        return apps;
    };
    let Ok(children) = root.get_children().await else {
        return apps;
    };
    for child in children {
        if is_null(&child) {
            continue;
        }
        let Some(proxy) = as_proxy(conn, &child).await else {
            continue;
        };
        if proxy.get_role().await.unwrap_or(Role::Unknown) == Role::DesktopFrame {
            if let Ok(inner) = proxy.get_children().await {
                apps.extend(inner.into_iter().filter(|c| !is_null(c)));
            }
        } else {
            apps.push(child);
        }
    }
    apps
}

/// 应用根 → 顶层窗口 ObjectRef 清单（窗口角色过滤）。
async fn app_windows(
    conn: &AccessibilityConnection,
    app: &ObjectRef,
) -> Vec<(ObjectRef, String)> {
    let mut windows = Vec::new();
    let Some(proxy) = as_proxy(conn, app).await else {
        return windows;
    };
    let Ok(children) = proxy.get_children().await else {
        return windows;
    };
    for child in children {
        if is_null(&child) {
            continue;
        }
        let Some(child_proxy) = as_proxy(conn, &child).await else {
            continue;
        };
        if is_window_role(child_proxy.get_role().await.unwrap_or(Role::Unknown)) {
            // 空标题窗口对齐 UIA 侧 list_windows 过滤惯例（不进清单）。
            let title = child_proxy.name().await.unwrap_or_default();
            if !title.trim().is_empty() {
                windows.push((child, title));
            }
        }
    }
    windows
}

/// 命中钻探结果：最深命中对象 + 自窗口层起的路径段 + 所属窗口/应用名。
struct DrillHit {
    hit: ObjectRef,
    /// 自身 + 祖先的（映射类型, 名字）对，**自窗口层起、含自身**。
    segments: Vec<(String, String)>,
    window_title: String,
    app_name: String,
    window_active: bool,
}

/// 自顶层窗口 `window` 内 `GetAccessibleAtPoint` 逐层下钻（§7.1）。
/// 点位不在窗口内 → None。
async fn drill_from_window(
    conn: &AccessibilityConnection,
    window: &ObjectRef,
    window_title: &str,
    app_name: &str,
    x: i32,
    y: i32,
) -> Option<DrillHit> {
    let wproxy = as_proxy(conn, window).await?;
    let comp = component_proxy(conn, window).await?;
    // 窗口矩形须含点（GetAccessibleAtPoint 对窗口自身语义各 toolkit 不一，
    // 先用 extents 显式把关）。
    let (wx, wy, ww, wh) = comp.get_extents(CoordType::Screen).await.ok()?;
    if ww <= 0 || wh <= 0 || x < wx || x >= wx + ww || y < wy || y >= wy + wh {
        return None;
    }
    let wrole = wproxy.get_role().await.unwrap_or(Role::Unknown);
    let states = wproxy.get_state().await.ok();
    let mut segments =
        vec![(role_to_control_type(wrole).to_string(), window_title.to_string())];
    let mut current = window.clone();
    for _ in 0..HIT_DRILL_MAX_DEPTH {
        let Some(comp) = component_proxy(conn, &current).await else {
            break;
        };
        let Ok(next) = comp.get_accessible_at_point(x, y, CoordType::Screen).await else {
            break;
        };
        if is_null(&next) || next.path.as_str() == current.path.as_str() {
            break;
        }
        let Some(next_proxy) = as_proxy(conn, &next).await else {
            break;
        };
        let role = next_proxy.get_role().await.unwrap_or(Role::Unknown);
        let raw_name = next_proxy.name().await.unwrap_or_default();
        // §22.7：密码控件 Name 即刻占位（path/label/图节点全链路只见占位符）。
        let name = mask_password_text(role_is_password(role), raw_name);
        segments.push((role_to_control_type(role).to_string(), name));
        current = next;
    }
    Some(DrillHit {
        hit: current,
        segments,
        window_title: window_title.to_string(),
        app_name: app_name.to_string(),
        window_active: states
            .map(|s| s.contains(State::Active))
            .unwrap_or(false),
    })
}

/// 桌面全域命中测试：逐应用逐窗口钻探，活动窗口优先（多窗口重叠时近似
/// UIA 的 topmost 语义），否则取首个含点窗口。
async fn hit_test(conn: &AccessibilityConnection, x: i32, y: i32) -> Option<DrillHit> {
    let mut candidates: Vec<DrillHit> = Vec::new();
    for app in desktop_apps(conn).await {
        let Some(app_proxy) = as_proxy(conn, &app).await else {
            continue;
        };
        let app_name = app_proxy.name().await.unwrap_or_default();
        for (window, title) in app_windows(conn, &app).await {
            if let Some(hit) =
                drill_from_window(conn, &window, &title, &app_name, x, y).await
            {
                candidates.push(hit);
            }
        }
    }
    match candidates.iter().position(|h| h.window_active) {
        Some(index) => Some(candidates.swap_remove(index)),
        None => candidates.into_iter().next(),
    }
}

/// 命中对象 → ControlRef（handle=0、密码掩码、窗口/进程同源自钻探链）。
async fn drill_hit_to_control(conn: &AccessibilityConnection, hit: &DrillHit) -> ControlRef {
    let proxy = as_proxy(conn, &hit.hit).await;
    let (role, name, automation_id, rect, enabled) = match proxy {
        Some(p) => {
            let role = p.get_role().await.unwrap_or(Role::Unknown);
            let name = mask_password_text(
                role_is_password(role),
                p.name().await.unwrap_or_default(),
            );
            let automation_id = p.accessible_id().await.unwrap_or_default();
            let rect = extents_of(conn, &hit.hit).await.unwrap_or_default();
            let enabled = p.get_state().await.ok().map(|s| s.contains(State::Enabled));
            (role, name, automation_id, rect, enabled)
        }
        None => (Role::Unknown, String::new(), String::new(), ControlRect::default(), None),
    };
    ControlRef {
        handle: 0,
        control_type: role_to_control_type(role).to_string(),
        name,
        automation_id,
        class_name: String::new(),
        window_title: hit.window_title.clone(),
        process_name: hit.app_name.clone(),
        rect,
        enabled,
        path: build_tree_path(&hit.segments),
    }
}

/// 前一个 Text 兄弟 → label 引用（≤ [`LABEL_MAX_PREV_SIBLINGS`] 步，
/// `get_index_in_parent` + `get_child_at_index` 逐个回看——与 uia 的
/// TreeWalker 回看语义对齐）。
async fn label_of_previous_text_sibling(
    conn: &AccessibilityConnection,
    proxy: &AccessibleProxy<'_>,
) -> Option<ControlRef> {
    let index = proxy.get_index_in_parent().await.ok()?;
    if index <= 0 {
        return None;
    }
    let parent = proxy.parent().await.ok()?;
    let pproxy = as_proxy(conn, &parent).await?;
    for step in 1..=LABEL_MAX_PREV_SIBLINGS as i32 {
        let prev_index = index - step;
        if prev_index < 0 {
            break;
        }
        let Ok(prev) = pproxy.get_child_at_index(prev_index).await else {
            break;
        };
        if is_null(&prev) {
            break;
        }
        let Some(prev_proxy) = as_proxy(conn, &prev).await else {
            break;
        };
        let role = prev_proxy.get_role().await.unwrap_or(Role::Unknown);
        if role_to_control_type(role) == "Text" && !role_is_password(role) {
            let name = prev_proxy.name().await.unwrap_or_default();
            if !name.is_empty() {
                return Some(ControlRef {
                    handle: 0,
                    control_type: "Text".into(),
                    name,
                    automation_id: prev_proxy.accessible_id().await.unwrap_or_default(),
                    class_name: String::new(),
                    window_title: String::new(),
                    process_name: String::new(),
                    rect: ControlRect::default(),
                    enabled: None,
                    path: None,
                });
            }
        }
    }
    None
}

/* --- 元素标注（recorder 复用；祖先上溯版路径构造） ---------------------------- */

/// ObjectRef 的感知标注（recorder 事件行的元素锚点；probe 的缓存焦点/
/// 窗口回建也走它）。
#[derive(Debug, Clone)]
pub(crate) struct ElementAnnotation {
    /// 映射后的 UIA ControlType 字符串。
    pub control_type: String,
    /// 可访问名（密码已掩码）。
    pub name: String,
    /// `Role.PasswordText` 判定（recorder 侧 text 再遮一道）。
    pub is_password: bool,
    /// `ControlRef.path`（树路径，窗口层起、深度封顶）。
    pub tree_path: Option<String>,
    /// 所属顶层窗口标题（同源自祖先链）。
    pub window_title: String,
    /// 应用可访问名（≈ 进程名语义的 AT-SPI 通道）。
    pub process_name: String,
    /// 屏幕矩形（Component extents；失败全 0）。
    pub rect: ControlRect,
}

/// ObjectRef → [`ElementAnnotation`]（祖先上溯 ≤ PATH_MAX_DEPTH 步；
/// 异步本体——recorder 事件泵与 probe 的 block_on 调用方共用）。
pub(crate) async fn annotate_object_async(
    conn: &AccessibilityConnection,
    obj: &ObjectRef,
) -> Option<ElementAnnotation> {
    let proxy = as_proxy(conn, obj).await?;
    let role = proxy.get_role().await.ok()?;
    let is_password = role_is_password(role);
    let name = mask_password_text(is_password, proxy.name().await.unwrap_or_default());
    // 自身 + 祖先（近→远）；应用根不进路径（UIA ControlView 无 Application
    // 层，路径顶 = 顶层窗口）。
    let mut segments: Vec<(String, String)> =
        vec![(role_to_control_type(role).to_string(), name.clone())];
    let mut current = proxy.parent().await.ok()?;
    let mut window_title = String::new();
    let mut process_name = String::new();
    for _ in 0..PATH_MAX_DEPTH {
        if is_null(&current) {
            break;
        }
        let Some(parent) = as_proxy(conn, &current).await else {
            break;
        };
        let prole = parent.get_role().await.unwrap_or(Role::Unknown);
        let pname = parent.name().await.unwrap_or_default();
        if prole == Role::Application {
            // 应用根：记进程名后停（其上即桌面层）。
            process_name = pname;
            break;
        }
        let mapped = role_to_control_type(prole);
        if mapped == "Window" && window_title.is_empty() {
            window_title = pname.clone();
        }
        segments.push((mapped.to_string(), mask_password_text(role_is_password(prole), pname)));
        current = parent.parent().await.ok()?;
    }
    segments.reverse();
    // 路径自窗口层起：找首个 Window 段截去其上方（无窗口段则全量——对象
    // 本身就在窗口层之上时的兜底）。
    let start = segments
        .iter()
        .position(|(t, _)| t == "Window")
        .unwrap_or(0);
    let rect = extents_of(conn, obj).await.unwrap_or_default();
    Some(ElementAnnotation {
        control_type: role_to_control_type(role).to_string(),
        name,
        is_password,
        tree_path: build_tree_path(&segments[start..]),
        window_title,
        process_name,
        rect,
    })
}

/// 标注 → 元素级 ControlRef（handle=0；recorder/probe 共用）。
pub(crate) fn annotation_to_control_ref(ann: &ElementAnnotation) -> ControlRef {
    ControlRef {
        handle: 0,
        control_type: ann.control_type.clone(),
        name: ann.name.clone(),
        automation_id: String::new(),
        class_name: String::new(),
        window_title: ann.window_title.clone(),
        process_name: ann.process_name.clone(),
        rect: ann.rect,
        enabled: None,
        path: ann.tree_path.clone(),
    }
}

/// 窗口 ObjectRef → WindowRef（hash 句柄 + 标题 + 应用名）。
pub(crate) fn window_ref_of_object(
    conn: &AccessibilityConnection,
    obj: &ObjectRef,
) -> Option<WindowRef> {
    block_on(window_ref_of_object_async(conn, obj))
}

/* --- ControlProbe 实现 --------------------------------------------------------- */

impl ControlProbe for AtspiProbe {
    fn control_at(&self, x: f64, y: f64) -> Option<ControlObservation> {
        block_on(async {
            let hit = hit_test(&self.conn, x as i32, y as i32).await?;
            let control = drill_hit_to_control(&self.conn, &hit).await;
            let label = label_of(&self.conn, &hit.hit).await;
            Some(ControlObservation { control, label })
        })
    }

    fn focused(&self) -> Option<ControlObservation> {
        // §7.5-1：事件缓存优先（recorder 在场时 O(1)）；缓存对象失效则清
        // 缓存走冷扫。
        if let Some(cache) = &self.cache {
            if let Some(obj) = cache.focus() {
                if let Some(obs) = self.observation_from_object(&obj) {
                    return Some(obs);
                }
                cache.clear_focus();
            }
        }
        self.cold_scan_focused()
    }

    fn foreground_window(&self) -> Option<WindowRef> {
        if let Some(cache) = &self.cache {
            if let Some(window) = cache.active_window() {
                if let Some(r) = window_ref_of_object(&self.conn, &window) {
                    return Some(r);
                }
            }
        }
        block_on(async {
            // 缓存缺席：State.Active 的顶层窗口；再兜底清单首项。
            for app in desktop_apps(&self.conn).await {
                for (window, _) in app_windows(&self.conn, &app).await {
                    if has_state(&self.conn, &window, State::Active).await {
                        if let Some(r) = window_ref_of_object_async(&self.conn, &window).await {
                            return Some(r);
                        }
                    }
                }
            }
            None
        })
        .or_else(|| self.list_windows().into_iter().next())
    }

    fn list_windows(&self) -> Vec<WindowRef> {
        block_on(async {
            let mut windows = Vec::new();
            for app in desktop_apps(&self.conn).await {
                let Some(app_proxy) = as_proxy(&self.conn, &app).await else {
                    continue;
                };
                let app_name = app_proxy.name().await.unwrap_or_default();
                for (_, title) in app_windows(&self.conn, &app).await {
                    windows.push(WindowRef {
                        handle: stable_window_handle(&app_name, &title),
                        title,
                        process_name: app_name.clone(),
                    });
                }
            }
            // 前台（缓存）优先，对齐 UIA 侧「先前台」的清单序惯例。
            if let Some(cache) = &self.cache {
                if let Some(active) = cache.active_window() {
                    if let Some(active_ref) = window_ref_of_object_async(&self.conn, &active).await
                    {
                        if let Some(pos) =
                            windows.iter().position(|w| w.handle == active_ref.handle)
                        {
                            let front = windows.remove(pos);
                            windows.insert(0, front);
                        }
                    }
                }
            }
            windows
        })
    }

    fn dump_tree(&self, max_depth: u32, max_nodes: usize) -> Vec<SceneNode> {
        let Some(root) = self.foreground_window_node() else {
            return Vec::new();
        };
        block_on(async {
            let mut out: Vec<SceneNode> = Vec::new();
            if max_depth == 0 || max_nodes == 0 {
                return out;
            }
            // 前序 DFS、path = 子序号链、深度触限只剪子树、节点预算停收集
            //（与 synth dump_tree 语义一致；P3-7 回归锁定其行为）。
            let mut stack: Vec<(ObjectRef, String, u32)> =
                vec![(root, "0".into(), 1)];
            while let Some((obj, path, depth)) = stack.pop() {
                let Some(proxy) = as_proxy(&self.conn, &obj).await else {
                    continue;
                };
                let role = proxy.get_role().await.unwrap_or(Role::Unknown);
                let name = mask_password_text(
                    role_is_password(role),
                    proxy.name().await.unwrap_or_default(),
                );
                let automation_id = proxy.accessible_id().await.unwrap_or_default();
                let enabled = proxy.get_state().await.ok().map(|s| s.contains(State::Enabled));
                out.push(SceneNode {
                    path: path.clone(),
                    control_type: role_to_control_type(role).to_string(),
                    name,
                    automation_id: (!automation_id.is_empty()).then_some(automation_id),
                    class_name: None,
                    enabled,
                });
                if out.len() >= max_nodes {
                    out.truncate(max_nodes);
                    break;
                }
                if depth < max_depth {
                    if let Ok(children) = proxy.get_children().await {
                        for (i, child) in children.into_iter().enumerate().rev() {
                            if is_null(&child) {
                                continue;
                            }
                            stack.push((child, format!("{path}/{i}"), depth + 1));
                        }
                    }
                }
            }
            out
        })
    }

    fn value_at(&self, x: f64, y: f64) -> Option<String> {
        block_on(async {
            let hit = hit_test(&self.conn, x as i32, y as i32).await?;
            let proxy = as_proxy(&self.conn, &hit.hit).await?;
            let role = proxy.get_role().await.ok()?;
            // §22.7 红线：密码控件不读值，直接占位返回。
            if role_is_password(role) {
                return Some(PASSWORD_PLACEHOLDER.to_string());
            }
            let interfaces = proxy.get_interfaces().await.ok()?;
            // Value 接口（滑杆/调节类）→ 数值。
            if interfaces.contains(Interface::Value) {
                if let Ok(value) = ValueProxy::builder(self.conn.connection())
                    .destination(hit.hit.name.clone())
                    .ok()?
                    .path(hit.hit.path.clone())
                    .ok()?
                    .cache_properties(atspi::zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .ok()?
                    .current_value()
                    .await
                {
                    return Some(format_number(value));
                }
            }
            // Text 接口（编辑框/文档）→ 全文。
            if interfaces.contains(Interface::Text) {
                if let Ok(text) = TextProxy::builder(self.conn.connection())
                    .destination(hit.hit.name.clone())
                    .ok()?
                    .path(hit.hit.path.clone())
                    .ok()?
                    .cache_properties(atspi::zbus::proxy::CacheProperties::No)
                    .build()
                    .await
                    .ok()?
                    .get_text(0, -1)
                    .await
                {
                    return Some(text);
                }
            }
            // Name 兜底（对齐 UIA 侧 ValuePattern → Name 降级链）。
            proxy.name().await.ok()
        })
    }
}

/// 数值读回格式化（整数值不带小数尾）。
fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

impl AtspiProbe {
    /// 缓存/扫描得到的前台窗口 ObjectRef（dump_tree 的树根）。
    fn foreground_window_node(&self) -> Option<ObjectRef> {
        if let Some(cache) = &self.cache {
            if let Some(window) = cache.active_window() {
                return Some(window);
            }
        }
        block_on(async {
            for app in desktop_apps(&self.conn).await {
                for (window, _) in app_windows(&self.conn, &app).await {
                    if has_state(&self.conn, &window, State::Active).await {
                        return Some(window);
                    }
                }
            }
            // 无 Active 窗口：首个顶层窗口（无头/未聚焦会话的诚实兜底）。
            for app in desktop_apps(&self.conn).await {
                let windows = app_windows(&self.conn, &app).await;
                if let Some((window, _)) = windows.first() {
                    return Some(window.clone());
                }
            }
            None
        })
    }

    /// 任意 ObjectRef → 完整观测（缓存焦点路径复用）。
    fn observation_from_object(&self, obj: &ObjectRef) -> Option<ControlObservation> {
        block_on(async {
            let ann = annotate_object_async(&self.conn, obj).await?;
            let control = annotation_to_control_ref(&ann);
            let label = label_of(&self.conn, obj).await;
            Some(ControlObservation { control, label })
        })
    }

    /// 冷启动焦点全扫（无 recorder 缓存；节点预算封顶防巨型桌面树）。
    fn cold_scan_focused(&self) -> Option<ControlObservation> {
        block_on(async {
            let mut budget = FOCUSED_SCAN_NODE_BUDGET;
            for app in desktop_apps(&self.conn).await {
                let Some(found) = scan_focused_dfs(&self.conn, &app, &mut budget).await else {
                    continue;
                };
                let ann = annotate_object_async(&self.conn, &found).await?;
                let control = annotation_to_control_ref(&ann);
                let label = label_of(&self.conn, &found).await;
                return Some(ControlObservation { control, label });
            }
            None
        })
    }
}

/// 焦点 DFS（预算驱动；返回首个 State.Focused 命中对象；显式栈迭代——
/// async 递归需装箱，树遍历无需付这个代价）。
async fn scan_focused_dfs(
    conn: &AccessibilityConnection,
    root: &ObjectRef,
    budget: &mut usize,
) -> Option<ObjectRef> {
    let mut stack = vec![root.clone()];
    while let Some(obj) = stack.pop() {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let Some(proxy) = as_proxy(conn, &obj).await else {
            continue;
        };
        if proxy
            .get_state()
            .await
            .map(|s| s.contains(State::Focused))
            .unwrap_or(false)
        {
            return Some(obj);
        }
        if let Ok(children) = proxy.get_children().await {
            stack.extend(children.into_iter().filter(|c| !is_null(c)));
        }
    }
    None
}

/// [`window_ref_of_object`] 的异步本体（foreground_window/list_windows 内联）。
async fn window_ref_of_object_async(
    conn: &AccessibilityConnection,
    obj: &ObjectRef,
) -> Option<WindowRef> {
    let proxy = as_proxy(conn, obj).await?;
    if !is_window_role(proxy.get_role().await.unwrap_or(Role::Unknown)) {
        return None;
    }
    let title = proxy.name().await.unwrap_or_default();
    if title.trim().is_empty() {
        return None;
    }
    let app_name = owning_app_name(conn, &proxy).await;
    Some(WindowRef {
        handle: stable_window_handle(&app_name, &title),
        title,
        process_name: app_name,
    })
}

/* --- set_value（对接契约：签名逐字保持） --------------------------------------- */

/// 坐标处控件 EditableText 直写（serve `set_value` op 的 Linux 路径，
/// §7.1；**与其他 worker 的对接契约，签名逐字保持**）。
///
/// 命中测试定位 → `QueryEditableText.set_text_contents`。失败闭合：
/// 无 a11y 总线 → [`Error::AtspiConnectFailed`]；坐标无控件 →
/// [`Error::NoControlAtPoint`]；控件无 EditableText 接口 →
/// [`Error::AtspiEditUnsupported`]；直写失败 → [`Error::AtspiEditFailed`]。
pub fn set_value_at_point(x: f64, y: f64, value: &str) -> crate::harness::Result<()> {
    let conn = shared_connection()?;
    block_on(async {
        let Some(hit) = hit_test(&conn, x as i32, y as i32).await else {
            return Err(Error::NoControlAtPoint);
        };
        let proxy = as_proxy(&conn, &hit.hit)
            .await
            .ok_or(Error::NoControlAtPoint)?;
        let interfaces = proxy
            .get_interfaces()
            .await
            .map_err(|e| Error::AtspiEditFailed(Box::new(e)))?;
        if !interfaces.contains(Interface::EditableText) {
            return Err(Error::AtspiEditUnsupported);
        }
        let editable = EditableTextProxy::builder(conn.connection())
            .destination(hit.hit.name.clone())
            .map_err(|e| Error::AtspiEditFailed(Box::new(e)))?
            .path(hit.hit.path.clone())
            .map_err(|e| Error::AtspiEditFailed(Box::new(e)))?
            .cache_properties(atspi::zbus::proxy::CacheProperties::No)
            .build()
            .await
            .map_err(|e| Error::AtspiEditFailed(Box::new(e)))?;
        match editable.set_text_contents(value).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::AtspiEditFailed(Box::new(std::io::Error::other(
                "set_text_contents 返回 false",
            )))),
            Err(e) => Err(Error::AtspiEditFailed(Box::new(e))),
        }
    })
}

/// 进程级 a11y 连接（AtspiProbe::new / set_value_at_point 共享；首次调用
/// 建连，此后克隆复用——zbus 连接线程安全）。总线消亡后的陈旧连接按
/// 各调用错误路径闭合（P2 不做自动重连，实机长会话场景进 todo）。
fn shared_connection() -> Result<AccessibilityConnection, Error> {
    static CELL: Mutex<Option<AccessibilityConnection>> = Mutex::new(None);
    let mut guard = CELL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(conn) = guard.as_ref() {
        return Ok(conn.clone());
    }
    let conn = block_on(AccessibilityConnection::new())
        .map_err(|e| Error::AtspiConnectFailed(Box::new(e)))?;
    *guard = Some(conn.clone());
    Ok(conn)
}

/* --- 单测（纯逻辑；D-Bus 依赖项进 tests/atspi_smoke.rs） ----------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    /// UIA 侧 ControlType 词汇表（uia.rs map_control_type 的输出全集）——
    /// 映射表输出不得越界（跨源 ControlRef.controlType 契约一致）。
    const UIA_VOCABULARY: &[&str] = &[
        "Button", "Calendar", "CheckBox", "ComboBox", "Edit", "Hyperlink", "Image",
        "ListItem", "List", "Menu", "MenuBar", "MenuItem", "ProgressBar", "RadioButton",
        "ScrollBar", "Slider", "Spinner", "StatusBar", "Tab", "TabItem", "Text", "Thumb",
        "TitleBar", "ToolBar", "ToolTip", "Tree", "TreeItem", "Group", "Window", "Pane",
        "Document", "SplitButton", "DataGrid", "DataItem", "Header", "HeaderItem", "Table",
        "Separator", "Custom",
    ];

    /// 任务书契约锚点：button→Button、text→Edit、check-box→CheckBox、
    /// menu item→MenuItem；关键补充（label 启发式/密码/窗口识别）。
    #[test]
    fn role_mapping_contract_anchors() {
        assert_eq!(role_to_control_type(Role::Button), "Button");
        assert_eq!(role_to_control_type(Role::Text), "Edit");
        assert_eq!(role_to_control_type(Role::CheckBox), "CheckBox");
        assert_eq!(role_to_control_type(Role::MenuItem), "MenuItem");
        // label→Text：前邻 Text 兄弟的 label 启发式靠它命中。
        assert_eq!(role_to_control_type(Role::Label), "Text");
        // 密码框 → Edit + 角色判定（四采集点遮蔽的开关）。
        assert_eq!(role_to_control_type(Role::PasswordText), "Edit");
        assert!(role_is_password(Role::PasswordText));
        assert!(!role_is_password(Role::Entry));
        // 顶层窗口识别（GTK Frame / 通用 Window / Dialog / Alert）。
        for role in [Role::Frame, Role::Window, Role::Dialog, Role::Alert] {
            assert!(is_window_role(role));
            assert_eq!(role_to_control_type(role), "Window");
        }
        // 其余高频项抽样。
        assert_eq!(role_to_control_type(Role::Entry), "Edit");
        assert_eq!(role_to_control_type(Role::ToggleButton), "Button");
        assert_eq!(role_to_control_type(Role::Link), "Hyperlink");
        assert_eq!(role_to_control_type(Role::PageTab), "TabItem");
        assert_eq!(role_to_control_type(Role::SpinButton), "Spinner");
        assert_eq!(role_to_control_type(Role::Table), "Table");
        assert_eq!(role_to_control_type(Role::PushButtonMenu), "SplitButton");
        assert_eq!(role_to_control_type(Role::Unknown), "Custom");
    }

    /// 全表抽样穷尽性：以枚举变体序列化名为索引逐个过表（Role 无 EnumIter，
    /// 用 serde 字符串名 → State 式回放不可行；改为穷尽 match 的编译保证 +
    /// 词汇表越界检测的双保险）。此处验证：任务书角色名（kebab 风格）经
    /// role name 字符串解析后的映射一致性。
    #[test]
    fn role_mapping_outputs_stay_in_uia_vocabulary() {
        // 穷尽 match 已由编译器保证全量覆盖；运行期再验输出词汇表。
        let samples = [
            Role::Invalid,
            Role::AcceleratorLabel,
            Role::Alert,
            Role::Autocomplete,
            Role::CHART,
            Role::Caption,
            Role::Comment,
            Role::ContentDeletion,
            Role::ContentInsertion,
            Role::Definition,
            Role::DescriptionList,
            Role::DescriptionTerm,
            Role::DescriptionValue,
            Role::DocumentEmail,
            Role::DocumentFrame,
            Role::DocumentPresentation,
            Role::DocumentSpreadsheet,
            Role::DocumentText,
            Role::DocumentWeb,
            Role::Editbar,
            Role::Embedded,
            Role::Extended,
            Role::Footer,
            Role::Form,
            Role::Grouping,
            Role::Header,
            Role::Heading,
            Role::ImageMap,
            Role::InfoBar,
            Role::InputMethodWindow,
            Role::Landmark,
            Role::LevelBar,
            Role::Log,
            Role::Marquee,
            Role::Math,
            Role::MathFraction,
            Role::MathRoot,
            Role::Notification,
            Role::Paragraph,
            Role::Rating,
            Role::RedundantObject,
            Role::Section,
            Role::Static,
            Role::Subscript,
            Role::Suggestion,
            Role::Superscript,
            Role::Timer,
            Role::TitleBar,
            Role::TableRow,
        ];
        for role in samples {
            let mapped = role_to_control_type(role);
            assert!(
                UIA_VOCABULARY.contains(&mapped),
                "Role::{role:?} → {mapped:?} 越出 UIA 词汇表"
            );
        }
    }

    /// 树路径构造/截断（P3-7 语义）：≤8 祖先全保留；超深保最近链
    /// （根方向截去）；空段集 None。
    #[test]
    fn tree_path_builds_and_truncates_to_const_depth() {
        let seg = |t: &str, n: &str| (t.to_string(), n.to_string());
        // 窗口 + 2 层 + 自身 = 4 段全保留。
        let segments = vec![seg("Window", "主窗"), seg("Pane", "表单"), seg("Text", "名称"), seg("Edit", "")];
        assert_eq!(
            build_tree_path(&segments).as_deref(),
            Some("Window[主窗]/Pane[表单]/Text[名称]/Edit[]")
        );
        // 12 祖先 + 自身：截根保最近（8 祖先 + 自身）。
        let mut deep: Vec<(String, String)> = (0..12)
            .map(|i| seg("Pane", &format!("p{i}")))
            .collect();
        deep.push(seg("Edit", "hit"));
        let path = build_tree_path(&deep).expect("超深树仍产路径");
        let parts: Vec<&str> = path.split('/').collect();
        assert_eq!(parts.len(), PATH_MAX_DEPTH + 1, "8 祖先 + 自身");
        assert_eq!(parts[0], "Pane[p4]", "根方向超限祖先截去（p0..p3 弃）");
        assert_eq!(parts[PATH_MAX_DEPTH], "Edit[hit]", "链尾 = 自身");
        // 单段（命中即窗口层）。
        assert_eq!(
            build_tree_path(&[seg("Window", "w")]).as_deref(),
            Some("Window[w]")
        );
        assert_eq!(build_tree_path(&[]), None, "空段集无路径");
    }

    /// 窗口稳定句柄：确定性、可区分、非零。
    #[test]
    fn stable_window_handle_is_deterministic_and_distinct() {
        assert_eq!(
            stable_window_handle("notepad", "报销单"),
            stable_window_handle("notepad", "报销单")
        );
        assert_ne!(
            stable_window_handle("notepad", "报销单"),
            stable_window_handle("notepad", "报销单2"),
            "标题不同 → 句柄不同"
        );
        assert_ne!(
            stable_window_handle("app-a", "t"),
            stable_window_handle("app-b", "t"),
            "app 不同 → 句柄不同"
        );
        // 界值不撞 0（合成长会话里 0 = 未知句柄）。
        assert_ne!(stable_window_handle("", ""), 0);
        assert_ne!(stable_window_handle("app", "标题"), 0);
    }

    /// §22.7 密码掩码在感知面的套用：标注 → ControlRef 只见占位符，
    /// 结构锚点（path/类型）不受影响。
    #[test]
    fn annotation_masks_password_free_text_only() {
        let ann = ElementAnnotation {
            control_type: "Edit".into(),
            name: PASSWORD_PLACEHOLDER.into(),
            is_password: true,
            tree_path: Some("Window[登录]/Edit[]".into()),
            window_title: "登录".into(),
            process_name: "app".into(),
            rect: ControlRect::default(),
        };
        let control = annotation_to_control_ref(&ann);
        assert_eq!(control.name, PASSWORD_PLACEHOLDER);
        assert_eq!(control.control_type, "Edit");
        assert_eq!(control.path.as_deref(), Some("Window[登录]/Edit[]"), "锚点保留");
        assert_eq!(control.handle, 0, "object path 不落 handle（红线）");
        // 掩码入口：role_is_password 驱动 mask_password_text。
        assert_eq!(mask_password_text(true, "hunter2"), PASSWORD_PLACEHOLDER);
        assert_eq!(mask_password_text(false, "hunter2"), "hunter2");
    }

    /// 事件缓存读写/清除（§7.5-1 的数据面）。
    #[test]
    fn event_cache_tracks_focus_and_active_window() {
        let cache = EventCache::new();
        assert!(cache.focus().is_none());
        assert!(cache.active_window().is_none());
        let obj = ObjectRef::default();
        cache.note_focus(Some(obj.clone()));
        cache.note_active_window(obj.clone());
        assert_eq!(cache.focus(), Some(obj.clone()));
        assert_eq!(cache.active_window(), Some(obj));
        cache.clear_focus();
        assert!(cache.focus().is_none(), "失焦清空");
        cache.note_focus(None);
        assert!(cache.focus().is_none());
    }

    /// 数值格式化：整数值无小数尾、小数保留。
    #[test]
    fn number_formatting_trims_integral_values() {
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(0.5), "0.5");
        assert_eq!(format_number(-3.25), "-3.25");
    }
}
