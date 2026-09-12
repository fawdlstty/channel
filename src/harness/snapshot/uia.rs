//! 真实 UIA 感知（H1 生产实现，仅 Windows 目标编译）。
//!
//! 实现要点：
//! - **线程首次调用** `CoInitializeEx(MTA)` + 逐调用 `CoCreateInstance(
//!   CUIAutomation)`：UIA 客户端 COM 指针不跨线程持有（接口非 Send），COM
//!   逐线程一次性初始化、UIA 对象逐调用创建，换来无状态 `UiaProbe`
//!   （unit struct），可在任意线程/阻塞池直接使用；开销 ~1ms，相对点击
//!   频率可忽略（见 [`automation`] 的线程模型说明）；
//! - 语义与合成树（[`super::synth`]）对齐：命中元素、前邻 Text 兄弟作
//!   label（回看 ≤ [`super::synth::LABEL_MAX_PREV_SIBLINGS`] 步）、
//!   `path` 为祖先链 `Type[name]`（深度 ≤ [`super::synth::PATH_MAX_DEPTH`]，
//!   常量由 synth 统一定义）、`dump_tree` 深度/节点数双上限（深度达限只
//!   剪该子树、节点预算用尽停收集，不丢其他分支）；
//! - §22.7 红线落点：`IsPassword` 控件的自由文本（Name/读值/树转储）一律
//!   以 [`super::PASSWORD_PLACEHOLDER`] 占位，真实值不出 UIA 层；
//! - 所有属性读取**逐字段容错**（单属性失败 → 缺省值，不废整条观测）；
//!   只有元素定位本身失败才返回 None（与 app 既有 best-effort 同语义）。

use windows::core::{Interface, PWSTR};
use std::cell::Cell;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, UIA_AutomationIdPropertyId,
    UIA_ButtonControlTypeId, UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ClassNamePropertyId, UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId,
    UIA_DataGridControlTypeId, UIA_DataItemControlTypeId, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_GroupControlTypeId, UIA_HeaderControlTypeId,
    UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_IsEnabledPropertyId, UIA_IsPasswordPropertyId, UIA_ListControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuBarControlTypeId, UIA_MenuControlTypeId,
    UIA_MenuItemControlTypeId, UIA_NamePropertyId, UIA_PaneControlTypeId,
    UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId,
    UIA_SeparatorControlTypeId, UIA_SliderControlTypeId, UIA_SpinnerControlTypeId,
    UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId, UIA_TabControlTypeId,
    UIA_TabItemControlTypeId, UIA_TableControlTypeId, UIA_TextControlTypeId,
    UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId, UIA_ToolBarControlTypeId,
    UIA_ToolTipControlTypeId, UIA_TreeControlTypeId, UIA_TreeItemControlTypeId,
    UIA_WindowControlTypeId, UIA_ValuePatternId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible, GA_ROOT,
};

use super::synth::{LABEL_MAX_PREV_SIBLINGS, PATH_MAX_DEPTH};
use super::{mask_password_text, ControlObservation, ControlProbe, WindowRef, PASSWORD_PLACEHOLDER};
use crate::harness::store::{ControlRect, ControlRef, SceneNode};
use crate::harness::Error;

/// 生产 UIA 探测（无状态；见模块文档的逐调用 COM 策略）。
#[derive(Debug, Clone, Copy, Default)]
pub struct UiaProbe;

impl UiaProbe {
    /// 初始化探测（验证 COM/UIA 可用性；失败 = 环境无 UIA，调用方降级）。
    pub fn new() -> Result<Self, Error> {
        automation().map_err(Error::UiaInitFailed)?;
        Ok(Self)
    }
}

thread_local! {
    /// 本线程 COM 已完成（或已尝试）MTA 初始化（每线程一次，见
    /// [`ensure_com_initialized`]）。
    static COM_INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

/// 本线程 COM MTA 就绪（每线程首次调用时初始化一次，此后跳过）。
///
/// - **为什么不是进程级 OnceLock**：`CoInitializeEx` 的初始化单位是**线程**
///   （apartment 挂在线程上），进程级只执行一次会让其他线程的
///   `CoCreateInstance` 拿到 CO_E_NOTINITIALIZED；harness 的调用来自
///   tokio 阻塞池等多线程，必须逐线程保证。线程一次性即消除旧实现
///   「逐调用初始化」的引用累积。
/// - **为什么不配对 `CoUninitialize`**：MTA 的生命周期是进程级的，常驻
///   serve 逐调用初始化/反初始化会互相拆台（某线程的 CoUninitialize 可能
///   拆掉其他线程仍在使用的 apartment）；不配对则线程池线程与进程同寿命，
///   线程退出时 COM 自动清理，泄漏面为零。
/// - 已初始化（S_FALSE）/模式不符（RPC_E_CHANGED_MODE，进程被其他模块先
///   设成 STA）都直接继续——UIA 客户端在既有 apartment 下同样可用，与旧
///   实现的容错语义一致。
fn ensure_com_initialized() {
    COM_INITIALIZED.with(|init| {
        if !init.get() {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            // 即便返回 RPC_E_CHANGED_MODE 也标记为已尝试：该线程的 apartment
            // 模式不会再变，重试无意义。
            init.set(true);
        }
    });
}

/// 逐调用创建 UIA 客户端（本线程 COM MTA 就绪）。
fn automation() -> windows::core::Result<IUIAutomation> {
    ensure_com_initialized();
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
}

/// UIA ControlType 代码 → 契约名（未知 → "Custom" 兜底）。
/// windows crate 的 UIA_*ControlTypeId 常量为 CamelCase（外部元数据命名），
/// match 模式引用它们会触发 non_upper_case_globals，此处显式放行。
#[allow(non_upper_case_globals)]
fn map_control_type(code: UIA_CONTROLTYPE_ID) -> &'static str {
    match code {
        UIA_ButtonControlTypeId => "Button",
        UIA_CalendarControlTypeId => "Calendar",
        UIA_CheckBoxControlTypeId => "CheckBox",
        UIA_ComboBoxControlTypeId => "ComboBox",
        UIA_EditControlTypeId => "Edit",
        UIA_HyperlinkControlTypeId => "Hyperlink",
        UIA_ImageControlTypeId => "Image",
        UIA_ListItemControlTypeId => "ListItem",
        UIA_ListControlTypeId => "List",
        UIA_MenuControlTypeId => "Menu",
        UIA_MenuBarControlTypeId => "MenuBar",
        UIA_MenuItemControlTypeId => "MenuItem",
        UIA_ProgressBarControlTypeId => "ProgressBar",
        UIA_RadioButtonControlTypeId => "RadioButton",
        UIA_ScrollBarControlTypeId => "ScrollBar",
        UIA_SliderControlTypeId => "Slider",
        UIA_SpinnerControlTypeId => "Spinner",
        UIA_StatusBarControlTypeId => "StatusBar",
        UIA_TabControlTypeId => "Tab",
        UIA_TabItemControlTypeId => "TabItem",
        UIA_TextControlTypeId => "Text",
        UIA_ThumbControlTypeId => "Thumb",
        UIA_TitleBarControlTypeId => "TitleBar",
        UIA_ToolBarControlTypeId => "ToolBar",
        UIA_ToolTipControlTypeId => "ToolTip",
        UIA_TreeControlTypeId => "Tree",
        UIA_TreeItemControlTypeId => "TreeItem",
        UIA_GroupControlTypeId => "Group",
        UIA_WindowControlTypeId => "Window",
        UIA_PaneControlTypeId => "Pane",
        UIA_DocumentControlTypeId => "Document",
        UIA_SplitButtonControlTypeId => "SplitButton",
        UIA_DataGridControlTypeId => "DataGrid",
        UIA_DataItemControlTypeId => "DataItem",
        UIA_HeaderControlTypeId => "Header",
        UIA_HeaderItemControlTypeId => "HeaderItem",
        UIA_TableControlTypeId => "Table",
        UIA_SeparatorControlTypeId => "Separator",
        _ => "Custom",
    }
}

/// BSTR → 去空白 String（BSTR Deref 到 &[u16]）。
fn bstr_to_string(value: &windows::core::BSTR) -> String {
    String::from_utf16_lossy(value).trim().to_string()
}

fn rect_from(r: RECT) -> ControlRect {
    ControlRect {
        x: r.left as f64,
        y: r.top as f64,
        width: (r.right - r.left).max(0) as f64,
        height: (r.bottom - r.top).max(0) as f64,
    }
}

/// pid → 进程镜像名（如 "notepad.exe"；失败 → 空串）。
fn process_image_name(pid: i32) -> String {
    if pid <= 0 {
        return String::new();
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) else {
            return String::new();
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let name = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .map(|_| {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or_default().to_string()
        })
        .unwrap_or_default();
        let _ = CloseHandle(handle);
        name
    }
}

fn window_title_of(hwnd: HWND) -> String {
    if hwnd.is_invalid() {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// hwnd → 顶层祖先窗口（GA_ROOT；取不到时退化回自身）。
fn root_ancestor(hwnd: HWND) -> HWND {
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    if root.is_invalid() { hwnd } else { root }
}

/// hwnd → 顶层窗口引用（标题 + 进程名）。
fn window_ref_of(hwnd: HWND) -> WindowRef {
    let hwnd = root_ancestor(hwnd);
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    WindowRef {
        handle: hwnd.0 as i64,
        title: window_title_of(hwnd),
        process_name: process_image_name(pid as i32),
    }
}

/// 元素所属窗口解析（归属同源）：优先元素自身原生句柄；无句柄时沿
/// ControlView 树上溯找最近带原生句柄的祖先（HTML 内容等无句柄元素的
/// 近似），步数以 [`PATH_MAX_DEPTH`] 封顶（防坏 provider 的环）。找不到
/// → None——宁可 windowTitle 留空，也不借前台窗口（跨窗口不同源）。
fn owning_window(
    element: &IUIAutomationElement,
    direct: HWND,
    walker: Option<&IUIAutomationTreeWalker>,
) -> Option<HWND> {
    if !direct.is_invalid() {
        return Some(direct);
    }
    let walker = walker?;
    let mut current = element.clone();
    for _ in 0..PATH_MAX_DEPTH {
        let Ok(parent) = (unsafe { walker.GetParentElement(&current) }) else {
            break;
        };
        let hwnd = unsafe { parent.CurrentNativeWindowHandle() }.unwrap_or_default();
        if !hwnd.is_invalid() {
            return Some(hwnd);
        }
        current = parent;
    }
    None
}

/// 元素 → ControlRef（逐字段容错；窗口/进程信息**同源**——窗口标题取
/// 元素自身句柄（或 UIA 树上溯祖先）的顶层窗口，进程名取元素自身 pid，
/// 绝不借前台窗口；与 app input_capture 的既有分工一致——宿主碰 UIA，
/// 窗口上下文就地取）。密码控件的 Name 即刻占位（§22.7）。
fn element_to_control(automation: &IUIAutomation, element: &IUIAutomationElement) -> ControlRef {
    unsafe {
        let control_type = element
            .CurrentControlType()
            .map(map_control_type)
            .unwrap_or("Custom")
            .to_string();
        let raw_name = element.CurrentName().map(|n| bstr_to_string(&n)).unwrap_or_default();
        // §22.7 红线：密码控件自由文本（Name）一律占位——path/label/图节点
        // 全链路只见占位符，真实值不出 UIA 层。
        let is_password = element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false);
        let name = mask_password_text(is_password, raw_name);
        let automation_id =
            element.CurrentAutomationId().map(|n| bstr_to_string(&n)).unwrap_or_default();
        let class_name =
            element.CurrentClassName().map(|n| bstr_to_string(&n)).unwrap_or_default();
        let enabled = element.CurrentIsEnabled().ok().map(|b| b.as_bool());
        let rect = element.CurrentBoundingRectangle().map(rect_from).unwrap_or_default();
        let hwnd = element.CurrentNativeWindowHandle().unwrap_or_default();
        // path / label 都依赖 ControlViewWalker；walker 不可用则两者缺席。
        let walker = automation.ControlViewWalker().ok();
        let path = walker.as_ref().map(|walker| {
            build_path(walker, element, &control_type, &name)
        });
        let window_title = owning_window(element, hwnd, walker.as_ref())
            .map(|hwnd| window_title_of(root_ancestor(hwnd)))
            .unwrap_or_default();
        let process_name =
            element.CurrentProcessId().map(process_image_name).unwrap_or_default();
        ControlRef {
            handle: if hwnd.is_invalid() { 0 } else { hwnd.0 as i64 },
            control_type,
            name,
            automation_id,
            class_name,
            window_title,
            process_name,
            rect,
            enabled,
            path,
        }
    }
}

/// 祖先链 `Type[name]/…/Type[name]`（含自身；根方向截断到 [`PATH_MAX_DEPTH`]，
/// 桌面根不进路径避免巨长前缀）。
fn build_path(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    control_type: &str,
    name: &str,
) -> String {
    let mut chain: Vec<(String, String)> = vec![(control_type.to_string(), name.to_string())];
    let mut current = element.clone();
    for _ in 0..PATH_MAX_DEPTH {
        let parent = unsafe { walker.GetParentElement(&current) };
        let Ok(parent) = parent else {
            break;
        };
        // 父再无父（桌面根）即停：根本身不进链。
        let grand = unsafe { walker.GetParentElement(&parent) };
        if grand.is_err() {
            break;
        }
        let segment = unsafe {
            let t = parent
                .CurrentControlType()
                .map(map_control_type)
                .unwrap_or("Custom")
                .to_string();
            let n = parent.CurrentName().map(|n| bstr_to_string(&n)).unwrap_or_default();
            (t, n)
        };
        chain.push(segment);
        current = parent;
    }
    chain.reverse();
    chain.into_iter().map(|(t, n)| format!("{t}[{n}]")).collect::<Vec<_>>().join("/")
}

/// 前邻 Text 兄弟 → label 引用（≤ [`LABEL_MAX_PREV_SIBLINGS`] 步）。
/// `automation` 为调用方已持有的 UIA 实例（P3-8：不再内部重建）。
fn label_of_previous_text_sibling(
    automation: &IUIAutomation,
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
) -> Option<ControlRef> {
    let mut current = element.clone();
    for _ in 0..LABEL_MAX_PREV_SIBLINGS {
        let prev = (unsafe { walker.GetPreviousSiblingElement(&current) }).ok()?;
        let control_type = (unsafe { prev.CurrentControlType() })
            .map(map_control_type)
            .unwrap_or("Custom");
        if control_type == "Text" {
            let name = (unsafe { prev.CurrentName() })
                .map(|n| bstr_to_string(&n))
                .unwrap_or_default();
            if !name.is_empty() {
                return Some(element_to_control(automation, &prev));
            }
        }
        current = prev;
    }
    None
}

/// 坐标处控件 ValuePattern 直写（serve `set_value` op，v1.11 操作面 Q）。
/// 与感知同款逐调用 COM 策略；不支持 ValuePattern / 坐标无控件 → Err。
pub fn set_value_at_point(x: f64, y: f64, text: &str) -> Result<(), Error> {
    let automation = automation().map_err(Error::UiaInitFailed)?;
    unsafe {
        let element = automation
            .ElementFromPoint(POINT { x: x as i32, y: y as i32 })
            .map_err(Error::ElementFromPointFailed)?;
        let unknown = element
            .GetCurrentPattern(UIA_ValuePatternId)
            .map_err(Error::ValuePatternUnsupported)?;
        let pattern: IUIAutomationValuePattern =
            unknown.cast().map_err(Error::ValuePatternUnsupported)?;
        let value = windows::core::BSTR::from(text);
        pattern.SetValue(&value).map_err(Error::SetValueFailed)
    }
}

/// 坐标处控件 ValuePattern 读值（serve `get_value` op，v1.11 操作面配套的
/// 只读回读；与 [`set_value_at_point`] 成对）。不支持 ValuePattern 时回退
/// Name；无控件 → None。
pub fn value_at_point(x: f64, y: f64) -> Option<String> {
    let automation = automation().ok()?;
    unsafe {
        let element = automation.ElementFromPoint(POINT { x: x as i32, y: y as i32 }).ok()?;
        // §22.7 红线：密码控件不读值，直接占位返回（真实值不跨 COM 边界）。
        if element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) {
            return Some(PASSWORD_PLACEHOLDER.to_string());
        }
        if let Ok(unknown) = element.GetCurrentPattern(UIA_ValuePatternId) {
            if let Ok(pattern) = unknown.cast::<IUIAutomationValuePattern>() {
                if let Ok(value) = pattern.CurrentValue() {
                    return Some(bstr_to_string(&value));
                }
            }
        }
        // ValuePattern 不可用/失败 → Name 兜底（标准 EDIT 控件 Name 即内容）。
        if let Ok(name) = element.CurrentName() {
            return Some(bstr_to_string(&name));
        }
        None
    }
}

impl ControlProbe for UiaProbe {
    fn control_at(&self, x: f64, y: f64) -> Option<ControlObservation> {
        let automation = automation().ok()?;
        let element =
            (unsafe { automation.ElementFromPoint(POINT { x: x as i32, y: y as i32 }) }).ok()?;
        let control = element_to_control(&automation, &element);
        let label = (unsafe { automation.ControlViewWalker() })
            .ok()
            .and_then(|walker| label_of_previous_text_sibling(&automation, &walker, &element));
        Some(ControlObservation { control, label })
    }

    fn value_at(&self, x: f64, y: f64) -> Option<String> {
        value_at_point(x, y)
    }

    fn focused(&self) -> Option<ControlObservation> {
        let automation = automation().ok()?;
        let element = (unsafe { automation.GetFocusedElement() }).ok()?;
        let control = element_to_control(&automation, &element);
        let label = (unsafe { automation.ControlViewWalker() })
            .ok()
            .and_then(|walker| label_of_previous_text_sibling(&automation, &walker, &element));
        Some(ControlObservation { control, label })
    }

    fn foreground_window(&self) -> Option<WindowRef> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.is_invalid()).then(|| window_ref_of(hwnd))
    }

    fn list_windows(&self) -> Vec<WindowRef> {
        let mut handles: Vec<isize> = Vec::new();
        let lparam = LPARAM(&mut handles as *mut _ as isize);
        unsafe {
            // 枚举失败（极少见）按空清单处理；单窗口信息读取失败逐条降级。
            let _ = EnumWindows(Some(enumerate_visible), lparam);
            // 接口文档承诺「按 z 序，先前台」：EnumWindows 的 z 序是置顶在
            // 前（≠ 前台在前）——显式把当前前台窗口提到首位（P3-10），
            // 其余保持 z 序；前台不在可见枚举清单时不重排。
            let fg = GetForegroundWindow();
            if !fg.is_invalid() {
                if let Some(pos) = handles.iter().position(|&h| h == fg.0 as isize) {
                    let hwnd = handles.remove(pos);
                    handles.insert(0, hwnd);
                }
            }
        }
        handles
            .into_iter()
            .map(|h| window_ref_of(HWND(h as *mut _)))
            .filter(|w| !w.title.is_empty())
            .collect()
    }

    fn dump_tree(&self, max_depth: u32, max_nodes: usize) -> Vec<SceneNode> {
        let Some(automation) = automation().ok() else {
            return Vec::new();
        };
        // 属性批量缓存（P3-9）：CacheRequest 让 6 个字段随元素枚举**一次**
        // 跨进程批量拉取（此后 CachedXxx 读本地缓存，无 RPC），替代旧实现
        // 每节点 ~6 次 Current* 逐属性 RPC；TreeWalker 的 *BuildCache 变体
        // 在枚举返回元素时附带缓存。
        let Ok(cache) = (unsafe { automation.CreateCacheRequest() }) else {
            return Vec::new();
        };
        for property in [
            UIA_ControlTypePropertyId,
            UIA_NamePropertyId,
            UIA_IsPasswordPropertyId,
            UIA_AutomationIdPropertyId,
            UIA_ClassNamePropertyId,
            UIA_IsEnabledPropertyId,
        ] {
            if unsafe { cache.AddProperty(property) }.is_err() {
                return Vec::new();
            }
        }
        // 根 = 前台窗口（无前台 → 桌面根）；与后续枚举同批带缓存。
        let fg = unsafe { GetForegroundWindow() };
        let Ok(walker) = (unsafe { automation.ControlViewWalker() }) else {
            return Vec::new();
        };
        let Ok(root) = (unsafe { automation.ElementFromHandleBuildCache(fg, &cache) })
            .or_else(|_| unsafe { automation.GetRootElementBuildCache(&cache) })
        else {
            return Vec::new();
        };
        // 待访问帧：孩子元素 + 父内序号 + 深度。兄弟链在**访问时**推进
        // （GetNextSiblingElementBuildCache）——不预先物化整个 children
        // Vec（P3-9），巨宽兄弟列不再有中间 Vec 峰值。
        struct Frame {
            element: IUIAutomationElement,
            index: usize,
            parent_path: String,
            depth: u32,
        }
        let mut out: Vec<SceneNode> = Vec::new();
        let mut stack: Vec<Frame> =
            vec![Frame { element: root, index: 0, parent_path: String::new(), depth: 1 }];
        while let Some(Frame { element, index, parent_path, depth }) = stack.pop() {
            let path = if parent_path.is_empty() {
                "0".to_string()
            } else {
                format!("{parent_path}/{index}")
            };
            let scene_node = unsafe {
                let control_type = element
                    .CachedControlType()
                    .map(map_control_type)
                    .unwrap_or("Custom")
                    .to_string();
                let raw_name =
                    element.CachedName().map(|n| bstr_to_string(&n)).unwrap_or_default();
                // §22.7 红线：树转储同样遮蔽密码控件 Name。
                let is_password =
                    element.CachedIsPassword().map(|b| b.as_bool()).unwrap_or(false);
                let automation_id = element
                    .CachedAutomationId()
                    .map(|n| bstr_to_string(&n))
                    .unwrap_or_default();
                let class_name = element
                    .CachedClassName()
                    .map(|n| bstr_to_string(&n))
                    .unwrap_or_default();
                let enabled = element.CachedIsEnabled().ok().map(|b| b.as_bool());
                SceneNode {
                    path: path.clone(),
                    control_type,
                    name: mask_password_text(is_password, raw_name),
                    automation_id: (!automation_id.is_empty()).then_some(automation_id),
                    class_name: (!class_name.is_empty()).then_some(class_name),
                    enabled,
                }
            };
            out.push(scene_node);
            if out.len() >= max_nodes {
                out.truncate(max_nodes);
                break; // 节点预算用尽：停止收集（其余分支无需再产出）。
            }
            // 前序 DFS、兄弟正序：兄弟先压（后弹出）、孩子后压（先弹出）。
            // 深度达上限不压孩子 → 只剪该子树，其余分支照常遍历；根
            // （depth 1）不推进兄弟（不越界到其他顶层窗口，与旧行为一致）。
            if depth > 1 {
                if let Ok(next) =
                    unsafe { walker.GetNextSiblingElementBuildCache(&element, &cache) }
                {
                    stack.push(Frame {
                        element: next,
                        index: index + 1,
                        parent_path: parent_path.clone(),
                        depth,
                    });
                }
            }
            if depth < max_depth {
                if let Ok(child) =
                    unsafe { walker.GetFirstChildElementBuildCache(&element, &cache) }
                {
                    stack.push(Frame {
                        element: child,
                        index: 0,
                        parent_path: path,
                        depth: depth + 1,
                    });
                }
            }
        }
        out
    }
}

/// EnumWindows 回调（收集可见顶层窗口句柄；BOOL true = 继续枚举）。
unsafe extern "system" fn enumerate_visible(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let list = &mut *(lparam.0 as *mut Vec<isize>);
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        list.push(hwnd.0 as isize);
    }
    true.into()
}
