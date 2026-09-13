//! crate 级错误类型（P3-13：错误类型从 String 收敛为枚举）。
//!
//! # 设计要点
//!
//! - **单一扁平枚举**（而非按模块子枚举）：所有错误最终都汇到 serve/exec
//!   的 wire 文案（`{"ok":false,"error":"…"}`），扁平化让「一个错误现场
//!   = 一个变体」可 grep、可单测锁定，避免错误在层间转译时丢语义；
//! - **Display 即 wire 契约**：serve 协议把错误以字符串写进回包（app 侧
//!   vitest / golden 双轨锁定），每个变体的 `#[error]` 文案必须逐字节等于
//!   原 `format!`/字符串字面量——等价性由本文件单测逐条重放原表达式断言
//!   （见 [`wire_displays_match_legacy_string_forms`] 等测试）；
//! - **外部错误经 `#[source]` 收敛**：`std::io::Error` / `serde_json::Error`
//!   / `windows::core::Error`（HRESULT）作为变体字段链式携带而非提前
//!   stringify——保留错误链（`source()` 可遍历），Display 仍复现原文案。
//!   `#[from]` 只放在 [`Error::BadRequest`]：serve 坏行是 JSON 错误的
//!   主场景且语义无歧义；其余场景的 io/json 错误都带上下文前缀
//!   （「写临时文件失败： …」「control-graph.json 损坏： …」），盲目
//!   `?` 自动转换会丢前缀破坏 wire，故一律显式 `map_err` 带上下文；
//! - **Windows 专属变体 `#[cfg(windows)]` 门控**：与使用它们的实现
//!   （snapshot/uia、snapshot/screen、actuate/sendinput）同生死，非
//!   Windows 目标不引用 `windows` crate。
//!
//! 注意：新增错误现场时先加变体、再写调用点——不要用 String 拼接绕过
//! （那是 P3-13 之前的旧病）。

use std::io;

/// crate 统一错误（变体按产生模块分区；Display 文案 = serve wire 文案）。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /* --- serve 协议层（src/serve） ------------------------------------------ */

    /// 请求行 JSON 解析失败（serve 坏行路径；`bad request: ` 前缀是 app 侧
    /// 认得的既定文案）。JSON 错误的 `#[from]` 收敛点。
    #[error("bad request: {0}")]
    BadRequest(#[from] serde_json::Error),

    /// 重 op 工作线程 panic（P3-5 归因：与超时区分；载荷为 panic 消息）。
    #[error("op panicked: {0}")]
    OpPanicked(String),

    /// 重 op 超时（v1.14 超时隔离：主循环限时等待失败，放弃工作线程）。
    #[error("操作超时（上限 {limit_ms} ms）：op 未在限时内完成，已放弃等待（挂起工作线程不回收，避免冻结 serve）")]
    OpTimeout { limit_ms: u128 },

    /// stdin 单行超过字节上限（P3-6b：整行丢弃，服务继续）。
    #[error("请求行超过 {limit} 字节上限，已丢弃该行（服务继续）")]
    OversizeLine { limit: usize },

    /// subscribe 缺 kinds 参数。
    #[error("subscribe 需要 {{\"kinds\":[...]}}")]
    SubscribeKindsMissing,

    /// subscribe 的 kinds 元素非字符串。
    #[error("kinds 元素必须为字符串")]
    SubscribeKindNotString,

    /// subscribe 的 kind 不在白名单（可用清单以尾随格式参数引用
    /// serve::KNOWN_EVENT_KINDS 的 Debug 形态——白名单扩充时 Display 自动
    /// 跟随，等价性单测对照同一常量锁定，防漂移）。
    #[error("unknown kind: {kind}（可用: {:?}）", crate::harness::serve::KNOWN_EVENT_KINDS)]
    UnknownEventKind { kind: String },

    /* --- tools 工具面（src/tools；参数校验文案直出 serve 响应） ------------ */

    /// 感知类 op 在探测不可用（无 UIA / 非 Windows）时的失败闭合。
    #[error("snapshot 探测不可用（无 UIA / 非 Windows 平台）")]
    ProbeUnavailable,

    /// 操作类 op 未授权（§22.7 授权门；文案与 tools::ACTUATION_LOCKED_MESSAGE
    /// 一致，等价性由单测对照该常量锁定）。
    #[error("操作类工具未授权：需以 harness --actuation 启动（或环境变量 HARNESS_ACTUATION=1）")]
    ActuationLocked,

    /// get_focus 无键盘焦点控件。
    #[error("无焦点控件")]
    NoFocusedControl,

    /// get_control 坐标未命中控件。
    #[error("该坐标无控件")]
    NoControlAtPoint,

    /// get_value 坐标无控件或不支持取值。
    #[error("该坐标无控件或不支持取值")]
    NoValueAtPoint,

    /// capture_scene 的 SceneSnapshot 序列化失败（serde 不透明错误）。
    #[error("Scene 序列化失败: {0}")]
    SceneSerialize(#[source] serde_json::Error),

    /// capture_scene 无前台窗口（会话无交互桌面）。
    #[error("无前台窗口，Scene 采集失败")]
    NoForegroundWindow,

    /// 未知 op（文案列出双侧工具清单与授权提示；调用方据此自纠）。
    #[error("unknown op: {0}（可用: 感知 list_windows/get_focus/get_control/get_value/dump_tree/capture_scene/capture_screen；操作 click/drag/type_text/key/set_value——后者需 --actuation 授权，§22.7）")]
    UnknownOp(String),

    /// get_control/click/set_value 缺 x/y 坐标（历史文案统一指 get_control）。
    #[error("get_control 需要 {{\"x\":number,\"y\":number}}")]
    MissingXY,

    /// capture_scene 的 x/y 只给其一。
    #[error("capture_scene 的 x/y 必须成对给出")]
    SceneXYPartial,

    /// capture_screen 的区域参数非整数（P3-12）。
    #[error("capture_screen 的 {field} 须为整数（收到: {value}）")]
    ScreenFieldNotInt { field: String, value: serde_json::Value },

    /// capture_screen 的宽/高越界（1..=10000，P3-12）。
    #[error("capture_screen 的 {field} 须在 1..={cap}（收到 {value}）")]
    ScreenDimOutOfRange { field: String, value: i64, cap: i32 },

    /// capture_screen 的坐标超出 i32 范围（虚拟屏坐标系本身的宽度）。
    #[error("capture_screen 的 {axis} 超出坐标范围（{value}）")]
    ScreenCoordOutOfRange { axis: &'static str, value: i64 },

    /// capture_screen 的区域参数缺任一（四参全给或全省略，P3-12）。
    #[error("capture_screen 的 x/y/width/height 须四参全给（区域截图）或全省略（整屏）")]
    ScreenArgsPartial,

    /// click 的 button 不在 left/right 白名单。
    #[error("未知按键: {0}（可用: left/right）")]
    UnknownMouseButton(String),

    /// drag 缺起点坐标。
    #[error("drag 需要 {{\"fromX\":number,\"fromY\":number}}")]
    DragFromMissing,

    /// drag 缺终点坐标。
    #[error("drag 需要 {{\"toX\":number,\"toY\":number}}")]
    DragToMissing,

    /// drag 的 via 非二元数值数组。
    #[error("drag 的 via 须为 [[x,y],...]（二元数值数组）")]
    ViaNotArray,

    /// drag 的 via 第 index 个元素非法。
    #[error("drag 的 via[{index}] 须为 [x,y] 二元数值数组")]
    ViaPointInvalid { index: usize },

    /// type_text 缺 text 参数。
    #[error("type_text 需要 {{\"text\":\"…\"}}")]
    TypeTextMissing,

    /// type_text 的 text 为空串。
    #[error("type_text 的 text 不得为空")]
    TypeTextEmpty,

    /// set_value 缺 value。
    #[error("set_value 需要 {{\"value\":\"…\",\"x\":number,\"y\":number}}")]
    SetValueMissing,

    /// key 缺 combo 参数。
    #[error("key 需要 {{\"combo\":\"ctrl+s\"}}")]
    ComboMissing,

    /* --- actuate 组合键解析（src/actuate.rs，跨平台纯逻辑） ----------------- */

    /// 组合键为空串。
    #[error("组合键为空（示例: ctrl+s）")]
    ComboEmpty,

    /// 修饰键位之外的 token 出现在主键前。
    #[error("「{0}」不是修饰键（可用: ctrl/alt/shift/win）")]
    NotAModifier(String),

    /// 主键名不在 VK 表内。
    #[error("未知键名: {0}")]
    UnknownKeyName(String),

    /// 主键本身是修饰键（ctrl+alt 之类无法作为快捷键）。
    #[error("组合键主键不能是修饰键")]
    ModifierAsMainKey,

    /* --- store 落盘（src/store；原子写 tmp + rename 的各失败阶段） --------- */

    /// control-graph.json 读取失败（缺文件/IO 错误；path 已字符串化保 Display）。
    #[error("control-graph.json 读取失败（{path}）: {source}")]
    ControlGraphRead { path: String, #[source] source: io::Error },

    /// control-graph.json 损坏（反序列化失败）。
    #[error("control-graph.json 损坏: {0}")]
    ControlGraphCorrupt(#[source] serde_json::Error),

    /// 落盘前序列化失败（pretty JSON 生成阶段）。
    #[error("序列化失败: {0}")]
    SerializeFailed(#[source] serde_json::Error),

    /// 目标目录创建失败。
    #[error("建目录失败（{dir}）: {source}")]
    CreateDirFailed { dir: String, #[source] source: io::Error },

    /// tmp 临时文件创建/写入失败。
    #[error("写临时文件失败: {0}")]
    WriteTmpFailed(#[source] io::Error),

    /// tmp → 目标 rename 失败（原子写最后一步）。
    #[error("原子替换失败: {0}")]
    RenameFailed(#[source] io::Error),

    /* --- Linux 平台扩展（upgrade.md §10.3/§7.3；跨平台不 cfg 门控——错误枚举
     * 是 serve wire 的稳定面，与实现 feature 解耦，未启用 feature 时这些
     * 变体只是不被生产） --------------------------------------------- */

    /// Linux 无显示服务（纯 TTY：无 DISPLAY 且无 WAYLAND_DISPLAY）。
    /// 承 linux.md §4 的错误码指引语义——headless 回放宿主截屏诚实降级。
    #[error("无显示服务（纯 TTY/无 X11 与 Wayland 会话，截屏不可用）")]
    NoDisplayServer,

    /// Wayland 原生截屏需 xdg-desktop-portal（不可用/未授权/被拒绝）。
    #[error("Wayland 原生截屏需 xdg-desktop-portal ScreenCast 授权（portal 不可用或授权被拒）")]
    WaylandPortalUnavailable,

    /* --- Linux 平台扩展续（upgrade.md §7.1/§7.3：X11 截屏/注入/剪贴板；
     * 跨平台不 cfg 门控——错误枚举是 serve wire 的稳定面，与实现 feature
     * 解耦，未启用 feature 时这些变体只是不被生产） ------------------------ */

    /// X11 连接失败（`DISPLAY` 在场但 X server 不可达：socket 权限/拼写
    /// 错误/XWayland 缺席等）。
    #[error("X11 连接失败（{0}）")]
    X11ConnectFailed(String),

    /// X11 请求阶段失败（GetImage/GetGeometry/XTest 等；`op` 标注阶段，
    /// `detail` 为 x11rb 错误的字符串形态——错误细节进 wire 而错误类型
    /// 不进公共枚举面；字段名避开 `source`（thiserror 会把该名当错误源
    /// 处理，String 非 Error 不满足约束））。
    #[error("X11 协议错误（{op}）: {detail}")]
    X11Protocol { op: &'static str, detail: String },

    /// X server 不支持 XTEST 扩展（键鼠注入不可用；截屏不受影响）。
    #[error("X server 不支持 XTEST 扩展（键鼠注入不可用；截屏不受影响）")]
    XTestUnavailable,

    /// 剪贴板工具缺失（非 ASCII 文本的粘贴路径不可用；xclip/wl-copy 运行时
    /// 探测全 miss）。
    #[error("剪贴板工具不可用（非 ASCII 键入需 xclip（X11）或 wl-copy（Wayland），未安装或不在 PATH）")]
    ClipboardUnavailable,

    /// 剪贴板读写子进程失败（工具在场但执行失败：spawn/stdin/非零退出）。
    #[error("剪贴板读写失败（{tool}: {detail}）")]
    ClipboardCommandFailed { tool: &'static str, detail: String },

    /// set_value 在当前 Linux 环境不可用（无 AT-SPI 感知，X11 协议面没有
    /// 坐标处控件直写通道——§7.3 能力矩阵「X11 无 AT-SPI」行）。
    #[error("set_value 不可用（Linux 下需 AT-SPI 控件树感知（feature linux-atspi）或剪贴板粘贴回退；当前均不可用）")]
    SetValueUnavailable,

    /// XTest 键入不支持的字符（`\n`/`\t`/退格/Escape 之外的 C0 控制符无
    /// X keysym 直打路径）。
    #[error("XTest 键入不支持的控制字符 U+{code:04X}（\\n/\\t/退格/Escape 外的 C0 控制符无 keysym）")]
    X11UnsupportedControlChar { code: u32 },

    /// 请求区域与 X11 根窗口空交（Linux 侧；对齐 Windows `ScreenRegionDisjoint`
    /// 语义——彼变体 cfg(windows)，错误枚举只追加故另立 Linux 形态）。
    #[error("请求区域与屏幕无交集（请求 ({x},{y}) {width}x{height}，屏幕 {vw}x{vh}）")]
    ScreenRegionDisjointX11 {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        vw: i32,
        vh: i32,
    },

    /// uinput 注入路线已 opt-in 但未实现（文档化占位，upgrade.md §7.1/§7.5-3：
    /// 绝对坐标注入需指针位置追踪（ydotool 走常驻 daemon）或 ABS 设备语义，
    /// Wayland 原生注入长期路线为 libei+RemoteDesktop portal）。
    #[error("uinput 注入路线未实现（绝对坐标注入需指针位置追踪（ydotool 为常驻 daemon）或 ABS 设备语义；Wayland 原生注入长期路线为 libei+RemoteDesktop portal，见 upgrade.md §7.1/§7.5）")]
    UinputNotImplemented,

    /// AT-SPI 总线连接/事件订阅失败（无 a11y 总线、D-Bus 不可达、注册被拒；
    /// 感知/事件面据此降级，浏览器 CDP 路线不受影响）。
    #[error("AT-SPI 连接失败（a11y 总线不可达或订阅失败）: {0}")]
    AtspiConnectFailed(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// 坐标处控件不支持 EditableText（AT-SPI set_value 直写通道缺席；
    /// 与 Windows 侧 ValuePatternUnsupported 同语义）。
    #[error("控件不支持 EditableText（无法直写值）")]
    AtspiEditUnsupported,

    /// EditableText 直写失败（set_text_contents 错误/被拒）。
    #[error("EditableText 直写失败: {0}")]
    AtspiEditFailed(#[source] Box<dyn std::error::Error + Send + Sync>),

    /* --- snapshot/screen（非 Windows 降级；Windows GDI/WIC 阶段） ---------- */

    /// 非 Windows 且非 Linux 平台（无 GDI/WIC、无 Linux 接线层）的抓屏
    /// 降级。Linux 已改走 [`crate::harness::snapshot::linux`]（错误
    /// `no-display-server`/`screen-unavailable-wayland-portal`）；变体
    /// 保留：错误枚举是 serve wire 的稳定面 + 等价性单测仍构造它
    /// （Display 文案契约不撤）。
    #[error("capture_screen 仅支持 Windows 平台（本机无 GDI/WIC，感知降级）")]
    ScreenNonWindows,

    /// 虚拟屏尺寸无效（宽/高 ≤0：无交互桌面会话）。
    #[cfg(windows)]
    #[error("虚拟屏尺寸无效（{width}x{height}，无交互桌面会话？）")]
    VirtualScreenInvalid { width: i32, height: i32 },

    /// 请求区域与虚拟屏空交（钳制后宽/高 <1）。
    #[cfg(windows)]
    #[error("请求区域与虚拟屏无交集（请求 ({x},{y}) {width}x{height}，虚拟屏 ({vx},{vy}) {vw}x{vh}）")]
    ScreenRegionDisjoint {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        vx: i32,
        vy: i32,
        vw: i32,
        vh: i32,
    },

    /// GetDC(屏幕) 失败。
    #[cfg(windows)]
    #[error("GetDC(屏幕) 失败")]
    GetScreenDcFailed,

    /// CreateCompatibleDC 失败。
    #[cfg(windows)]
    #[error("CreateCompatibleDC 失败")]
    CreateCompatibleDcFailed,

    /// CreateDIBSection 失败（内存 DIB 分配）。
    #[cfg(windows)]
    #[error("CreateDIBSection 失败: {0}")]
    CreateDibSectionFailed(#[source] windows::core::Error),

    /// BitBlt 抓屏失败（CAPTUREBLT 与 SRCCOPY 两形态都失败）。
    #[cfg(windows)]
    #[error("BitBlt 抓屏失败: {0}")]
    BitBltFailed(#[source] windows::core::Error),

    /// DIB 像素缓冲不可用（bits 指针为空）。
    #[cfg(windows)]
    #[error("DIB 像素缓冲不可用")]
    EmptyDibBuffer,

    /* --- snapshot/screen WIC PNG 编码各阶段（仅 Windows） ------------------- */

    /// WIC 工厂创建失败。
    #[cfg(windows)]
    #[error("WIC 工厂创建失败: {0}")]
    WicFactoryFailed(#[source] windows::core::Error),

    /// WIC 位图包装失败（CreateBitmapFromMemory）。
    #[cfg(windows)]
    #[error("WIC 位图包装失败: {0}")]
    WicBitmapFailed(#[source] windows::core::Error),

    /// IWICBitmapSource 转换失败（cast）。
    #[cfg(windows)]
    #[error("WIC 源转换失败: {0}")]
    WicSourceCastFailed(#[source] windows::core::Error),

    /// HGLOBAL 内存流创建失败。
    #[cfg(windows)]
    #[error("内存流创建失败: {0}")]
    MemoryStreamCreateFailed(#[source] windows::core::Error),

    /// PNG 编码器创建失败。
    #[cfg(windows)]
    #[error("PNG 编码器创建失败: {0}")]
    PngEncoderCreateFailed(#[source] windows::core::Error),

    /// 编码器初始化失败。
    #[cfg(windows)]
    #[error("编码器初始化失败: {0}")]
    EncoderInitFailed(#[source] windows::core::Error),

    /// 编码帧创建失败。
    #[cfg(windows)]
    #[error("帧创建失败: {0}")]
    FrameCreateFailed(#[source] windows::core::Error),

    /// 编码帧创建返回空（CreateNewFrame 成功但出参 None）。
    #[cfg(windows)]
    #[error("帧创建失败（空帧）")]
    FrameEmpty,

    /// 编码帧初始化失败。
    #[cfg(windows)]
    #[error("帧初始化失败: {0}")]
    FrameInitFailed(#[source] windows::core::Error),

    /// 编码帧尺寸设置失败。
    #[cfg(windows)]
    #[error("帧尺寸设置失败: {0}")]
    FrameSizeSetFailed(#[source] windows::core::Error),

    /// 像素写入 PNG 帧失败。
    #[cfg(windows)]
    #[error("PNG 写入失败: {0}")]
    PngWriteFailed(#[source] windows::core::Error),

    /// 编码帧提交失败。
    #[cfg(windows)]
    #[error("帧提交失败: {0}")]
    FrameCommitFailed(#[source] windows::core::Error),

    /// 编码器整体提交失败。
    #[cfg(windows)]
    #[error("编码提交失败: {0}")]
    EncoderCommitFailed(#[source] windows::core::Error),

    /// 从流取后备 HGLOBAL 失败。
    #[cfg(windows)]
    #[error("取后备内存失败: {0}")]
    HGlobalFromStreamFailed(#[source] windows::core::Error),

    /// 后备内存锁定失败（GlobalLock 空指针）。
    #[cfg(windows)]
    #[error("后备内存锁定失败")]
    WicLockFailed,

    /// 后备内存解锁失败（非 NO_ERROR 的成功例外路径）。
    #[cfg(windows)]
    #[error("后备内存解锁失败: {0}")]
    WicUnlockFailed(#[source] windows::core::Error),

    /* --- snapshot/uia（仅 Windows；逐调用 COM/UIA 的 HRESULT 失败） -------- */

    /// UIA 客户端初始化失败（CoInitializeEx/CoCreateInstance；探测降级）。
    #[cfg(windows)]
    #[error("UIA 初始化失败: {0}")]
    UiaInitFailed(#[source] windows::core::Error),

    /// ElementFromPoint 失败（坐标处控件定位）。
    #[cfg(windows)]
    #[error("该坐标无控件: {0}")]
    ElementFromPointFailed(#[source] windows::core::Error),

    /// 控件不支持 ValuePattern（GetCurrentPattern / cast 同文案）。
    #[cfg(windows)]
    #[error("控件不支持 ValuePattern: {0}")]
    ValuePatternUnsupported(#[source] windows::core::Error),

    /// UIA SetValue 直写失败。
    #[cfg(windows)]
    #[error("SetValue 失败: {0}")]
    SetValueFailed(#[source] windows::core::Error),

    /* --- actuate/sendinput（仅 Windows；键鼠注入/剪贴板各失败阶段） -------- */

    /// SetCursorPos 定位失败。
    #[cfg(windows)]
    #[error("SetCursorPos({x},{y}) 失败: {source}")]
    SetCursorPosFailed { x: i32, y: i32, #[source] source: windows::core::Error },

    /// SendInput 注入数量不完整（可能被前台窗口屏蔽）。
    #[cfg(windows)]
    #[error("SendInput 注入不完整（{injected}/{expected}，可能被前台窗口屏蔽）")]
    SendInputIncomplete { injected: usize, expected: usize },

    /// OpenClipboard 失败（其他进程持有剪贴板）。
    #[cfg(windows)]
    #[error("OpenClipboard 失败: {0}")]
    OpenClipboardFailed(#[source] windows::core::Error),

    /// EmptyClipboard 失败。
    #[cfg(windows)]
    #[error("EmptyClipboard 失败: {0}")]
    EmptyClipboardFailed(#[source] windows::core::Error),

    /// GlobalAlloc 失败（CF_UNICODETEXT 后备内存）。
    #[cfg(windows)]
    #[error("GlobalAlloc 失败: {0}")]
    GlobalAllocFailed(#[source] windows::core::Error),

    /// GlobalLock 失败（剪贴板置入路径）。
    #[cfg(windows)]
    #[error("GlobalLock 失败")]
    ClipboardLockFailed,

    /// SetClipboardData 失败。
    #[cfg(windows)]
    #[error("SetClipboardData 失败: {0}")]
    SetClipboardDataFailed(#[source] windows::core::Error),
}

/// 机器可读错误码（upgrade.md §10.3/§7.3 的加法式 `code` 扩展）：仅
/// **平台/能力级**错误携带（调用方可据此程序化分支：换路径重试、提示开
/// a11y、引导授权……）；参数校验/协议层错误以文案为准（人读语境明确，
/// 机器分支无增益）——返回 `None` 时 serve 错误响应不出现 `code` 键
/// （加法式，旧客户端不受影响）。
///
/// 清单（wire 契约，serve 错误响应单测 + golden errorCodeSample 锁定）：
/// `actuation-locked` / `no-display-server` / `screen-unavailable-wayland-portal` /
/// `x11-connect-failed` / `xtest-unavailable` / `clipboard-unavailable` /
/// `set-value-unavailable` / `uinput-unavailable` / `atspi-connect-failed`。
/// （v1.15：`browser-*`/`cdp-*`/`llm-*` 九个机器码随能力迁至 wr-harness-ext。）
impl Error {
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Error::ActuationLocked => Some("actuation-locked"),
            Error::NoDisplayServer => Some("no-display-server"),
            Error::WaylandPortalUnavailable => Some("screen-unavailable-wayland-portal"),
            Error::X11ConnectFailed(_) => Some("x11-connect-failed"),
            Error::XTestUnavailable => Some("xtest-unavailable"),
            Error::ClipboardUnavailable => Some("clipboard-unavailable"),
            Error::SetValueUnavailable => Some("set-value-unavailable"),
            Error::UinputNotImplemented => Some("uinput-unavailable"),
            Error::AtspiConnectFailed(_) => Some("atspi-connect-failed"),
            // 其余（参数校验/协议层/store/平台实现细节）无机器码。
            _ => None,
        }
    }
}

/// crate 统一 Result 别名（`Result<T>` 默认承载 [`Error`]；显式 E 覆盖
/// 仍可用，如 serve 内部纯 String 的历史路径已全部枚举化，无需覆盖）。
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    /// P3-13 wire 等价性（serve 协议层）：Display 必须逐字节等于原字符串
    /// 拼接——错误文案进 JSON-lines 回包，app 侧 vitest/调用方依赖既定
    /// 形态。右侧全部用**原实现的表达式**重放（format!/字面量），
    /// 枚举化若漂移即红。
    #[test]
    fn wire_displays_match_legacy_string_forms_serve() {
        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let expected = format!("bad request: {json_err}");
        assert_eq!(Error::BadRequest(json_err).to_string(), expected);
        // #[from] 收敛：serde_json::Error 直接 ?/From 进入即 BadRequest。
        let json_err2 = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        assert!(Error::from(json_err2).to_string().starts_with("bad request: "));

        let reason = "测试 panic（_test_panic）";
        assert_eq!(
            Error::OpPanicked(reason.to_string()).to_string(),
            format!("op panicked: {reason}")
        );
        assert_eq!(
            Error::OpTimeout { limit_ms: 250 }.to_string(),
            format!(
                "操作超时（上限 {} ms）：op 未在限时内完成，已放弃等待（挂起工作线程不回收，避免冻结 serve）",
                250u128
            )
        );
        assert_eq!(
            Error::OversizeLine { limit: crate::harness::serve::MAX_LINE_BYTES }.to_string(),
            format!("请求行超过 {} 字节上限，已丢弃该行（服务继续）", crate::harness::serve::MAX_LINE_BYTES)
        );
        assert_eq!(Error::SubscribeKindsMissing.to_string(), r#"subscribe 需要 {"kinds":[...]}"#);
        assert_eq!(Error::SubscribeKindNotString.to_string(), "kinds 元素必须为字符串");
        // 可用清单与 serve::KNOWN_EVENT_KINDS 常量对照（白名单扩充时防漂移）。
        assert_eq!(
            Error::UnknownEventKind { kind: "mouse".into() }.to_string(),
            format!("unknown kind: mouse（可用: {:?}）", crate::harness::serve::KNOWN_EVENT_KINDS)
        );
    }

    /// P3-13 wire 等价性（tools 工具面 + actuate 解析 + store 落盘）。
    #[test]
    fn wire_displays_match_legacy_string_forms_tools_store() {
        assert_eq!(
            Error::ProbeUnavailable.to_string(),
            "snapshot 探测不可用（无 UIA / 非 Windows 平台）"
        );
        // 授权门文案与 tools::ACTUATION_LOCKED_MESSAGE 常量对照（单测直接
        // 断言该常量的既有测试继续有效）。
        assert_eq!(Error::ActuationLocked.to_string(), crate::harness::tools::ACTUATION_LOCKED_MESSAGE);
        assert_eq!(Error::NoFocusedControl.to_string(), "无焦点控件");
        assert_eq!(Error::NoControlAtPoint.to_string(), "该坐标无控件");
        assert_eq!(Error::NoValueAtPoint.to_string(), "该坐标无控件或不支持取值");
        assert_eq!(Error::NoForegroundWindow.to_string(), "无前台窗口，Scene 采集失败");

        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let expected = format!("Scene 序列化失败: {json_err}");
        assert_eq!(Error::SceneSerialize(json_err).to_string(), expected);
        // unknown op 长文案（原 format! 行续接 `\` 去换行与前导空白后的
        // 实际字符串形态）。
        assert_eq!(
            Error::UnknownOp("nope".into()).to_string(),
            "unknown op: nope（可用: 感知 list_windows/get_focus/get_control/get_value/dump_tree/capture_scene/capture_screen；操作 click/drag/type_text/key/set_value——后者需 --actuation 授权，§22.7）"
        );

        assert_eq!(Error::MissingXY.to_string(), r#"get_control 需要 {"x":number,"y":number}"#);
        assert_eq!(Error::SceneXYPartial.to_string(), "capture_scene 的 x/y 必须成对给出");
        // serde_json::Value 的 Display（JSON 形态，字符串带引号）内嵌不变。
        assert_eq!(
            Error::ScreenFieldNotInt { field: "width".into(), value: serde_json::json!("abc") }.to_string(),
            format!("capture_screen 的 width 须为整数（收到: {}）", serde_json::json!("abc"))
        );
        let cap = crate::harness::tools::CAPTURE_SCREEN_MAX_DIMENSION;
        assert_eq!(
            Error::ScreenDimOutOfRange { field: "width".into(), value: 0, cap }.to_string(),
            format!("capture_screen 的 width 须在 1..={cap}（收到 0）")
        );
        assert_eq!(
            Error::ScreenCoordOutOfRange { axis: "x", value: 3_000_000_000i64 }.to_string(),
            format!("capture_screen 的 x 超出坐标范围（{}）", 3_000_000_000i64)
        );
        assert_eq!(
            Error::ScreenArgsPartial.to_string(),
            "capture_screen 的 x/y/width/height 须四参全给（区域截图）或全省略（整屏）"
        );
        assert_eq!(
            Error::UnknownMouseButton("mid".into()).to_string(),
            "未知按键: mid（可用: left/right）"
        );
        assert_eq!(Error::DragFromMissing.to_string(), r#"drag 需要 {"fromX":number,"fromY":number}"#);
        assert_eq!(Error::DragToMissing.to_string(), r#"drag 需要 {"toX":number,"toY":number}"#);
        assert_eq!(Error::ViaNotArray.to_string(), "drag 的 via 须为 [[x,y],...]（二元数值数组）");
        assert_eq!(
            Error::ViaPointInvalid { index: 3 }.to_string(),
            "drag 的 via[3] 须为 [x,y] 二元数值数组"
        );
        assert_eq!(Error::TypeTextMissing.to_string(), r#"type_text 需要 {"text":"…"}"#);
        assert_eq!(Error::TypeTextEmpty.to_string(), "type_text 的 text 不得为空");
        assert_eq!(
            Error::SetValueMissing.to_string(),
            r#"set_value 需要 {"value":"…","x":number,"y":number}"#
        );
        assert_eq!(Error::ComboMissing.to_string(), r#"key 需要 {"combo":"ctrl+s"}"#);

        // actuate 组合键解析。
        assert_eq!(Error::ComboEmpty.to_string(), "组合键为空（示例: ctrl+s）");
        assert_eq!(
            Error::NotAModifier("nope".into()).to_string(),
            "「nope」不是修饰键（可用: ctrl/alt/shift/win）"
        );
        assert_eq!(Error::UnknownKeyName("f25".into()).to_string(), "未知键名: f25");
        assert_eq!(Error::ModifierAsMainKey.to_string(), "组合键主键不能是修饰键");

        // store 落盘（io/json 错误非 Clone：各错误独立构造，Display 与
        // 同形构造的原表达式对照）。
        let io_err = std::io::Error::other("没有那个文件或目录 (os error 2)");
        let expected = format!("写临时文件失败: {io_err}");
        assert_eq!(Error::WriteTmpFailed(io_err).to_string(), expected);
        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let expected = format!("control-graph.json 损坏: {json_err}");
        assert_eq!(Error::ControlGraphCorrupt(json_err).to_string(), expected);
        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let expected = format!("序列化失败: {json_err}");
        assert_eq!(Error::SerializeFailed(json_err).to_string(), expected);
        assert!(
            Error::ControlGraphRead { path: "/tmp/x/ui/control-graph.json".into(), source: std::io::Error::other("boom") }
                .to_string()
                .starts_with("control-graph.json 读取失败（/tmp/x/ui/control-graph.json）: ")
        );
        assert!(
            Error::CreateDirFailed { dir: "/tmp/x/ui".into(), source: std::io::Error::other("boom") }
                .to_string()
                .starts_with("建目录失败（/tmp/x/ui）: ")
        );
        assert!(Error::RenameFailed(std::io::Error::other("boom")).to_string().starts_with("原子替换失败: "));
        // 非 Windows 抓屏降级。
        assert_eq!(
            Error::ScreenNonWindows.to_string(),
            "capture_screen 仅支持 Windows 平台（本机无 GDI/WIC，感知降级）"
        );
    }

    /// 错误链（#[source]）：外部错误作为 source 可遍历、可 downcast，
    /// 不提前 stringify（这是相对旧 String 形态的实质增益）。
    #[test]
    fn external_errors_stay_in_source_chain() {
        let err = Error::WriteTmpFailed(std::io::Error::other("boom"));
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "io 错误应保留在错误链 source() 上");
        assert!(source.unwrap().downcast_ref::<std::io::Error>().is_some());

        let json_err = serde_json::from_str::<serde_json::Value>("{oops").unwrap_err();
        let err = Error::SceneSerialize(json_err);
        assert!(std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<serde_json::Error>())
            .is_some());
    }

    /// 平台/能力级扩展变体（upgrade.md §10.3/§7.3）：Display 文案与
    /// code 映射逐条锁定（新变体同样要求等价性断言，红线）。文案是自拟的
    /// 新契约（无历史字符串可对照），用字面量锁定防后续无意识漂移。
    #[test]
    fn platform_capability_variants_display_and_code() {
        assert_eq!(
            Error::NoDisplayServer.to_string(),
            "无显示服务（纯 TTY/无 X11 与 Wayland 会话，截屏不可用）"
        );
        assert_eq!(
            Error::WaylandPortalUnavailable.to_string(),
            "Wayland 原生截屏需 xdg-desktop-portal ScreenCast 授权（portal 不可用或授权被拒）"
        );

        // code 映射（机器可读清单，见 Error::code 文档）。
        assert_eq!(Error::ActuationLocked.code(), Some("actuation-locked"));
        assert_eq!(Error::NoDisplayServer.code(), Some("no-display-server"));
        assert_eq!(
            Error::WaylandPortalUnavailable.code(),
            Some("screen-unavailable-wayland-portal")
        );

        // 其余变体无 code（参数校验/协议层——机器分支无增益）。
        assert_eq!(Error::ProbeUnavailable.code(), None);
        assert_eq!(Error::UnknownOp("x".into()).code(), None);
        assert_eq!(Error::BadRequest(serde_json::from_str::<serde_json::Value>("{oops").unwrap_err()).code(), None);
        assert_eq!(Error::OpTimeout { limit_ms: 1 }.code(), None);
    }

    /// Linux 平台扩展变体（upgrade.md §7.1/§7.3：X11/XTest/剪贴板/
    /// set_value/空交/uinput）：Display 文案与 code 映射逐条锁定（新变体
    /// 等价性断言红线；文案为自拟新契约，用字面量锁防无意识漂移）。
    #[test]
    fn linux_variants_display_and_code() {
        assert_eq!(
            Error::X11ConnectFailed("connection refused".into()).to_string(),
            "X11 连接失败（connection refused）"
        );
        assert_eq!(
            Error::X11Protocol { op: "GetImage", detail: "broken pipe".into() }.to_string(),
            "X11 协议错误（GetImage）: broken pipe"
        );
        assert_eq!(
            Error::XTestUnavailable.to_string(),
            "X server 不支持 XTEST 扩展（键鼠注入不可用；截屏不受影响）"
        );
        assert_eq!(
            Error::ClipboardUnavailable.to_string(),
            "剪贴板工具不可用（非 ASCII 键入需 xclip（X11）或 wl-copy（Wayland），未安装或不在 PATH）"
        );
        assert_eq!(
            Error::ClipboardCommandFailed { tool: "xclip", detail: "退出码 Some(1)".into() }
                .to_string(),
            "剪贴板读写失败（xclip: 退出码 Some(1)）"
        );
        assert_eq!(
            Error::SetValueUnavailable.to_string(),
            "set_value 不可用（Linux 下需 AT-SPI 控件树感知（feature linux-atspi）或剪贴板粘贴回退；当前均不可用）"
        );
        assert_eq!(
            Error::X11UnsupportedControlChar { code: 0x00 }.to_string(),
            "XTest 键入不支持的控制字符 U+0000（\\n/\\t/退格/Escape 外的 C0 控制符无 keysym）"
        );
        assert_eq!(
            Error::ScreenRegionDisjointX11 {
                x: -100,
                y: -100,
                width: 10,
                height: 10,
                vw: 1920,
                vh: 1080
            }
            .to_string(),
            "请求区域与屏幕无交集（请求 (-100,-100) 10x10，屏幕 1920x1080）"
        );
        assert!(Error::UinputNotImplemented.to_string().starts_with("uinput 注入路线未实现"));

        // code 映射（平台/能力级：宿主可程序化换路径/提示授权）。
        assert_eq!(
            Error::X11ConnectFailed(String::new()).code(),
            Some("x11-connect-failed")
        );
        assert_eq!(Error::XTestUnavailable.code(), Some("xtest-unavailable"));
        assert_eq!(Error::ClipboardUnavailable.code(), Some("clipboard-unavailable"));
        assert_eq!(Error::SetValueUnavailable.code(), Some("set-value-unavailable"));
        assert_eq!(Error::UinputNotImplemented.code(), Some("uinput-unavailable"));
        // 实现细节/参数级：无机器码（与 Windows 侧 ScreenRegionDisjoint 同例）。
        assert_eq!(Error::X11Protocol { op: "GetImage", detail: String::new() }.code(), None);
        assert_eq!(
            Error::ClipboardCommandFailed { tool: "xclip", detail: String::new() }.code(),
            None
        );
        assert_eq!(Error::X11UnsupportedControlChar { code: 0 }.code(), None);
        assert_eq!(
            Error::ScreenRegionDisjointX11 {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                vw: 0,
                vh: 0
            }
            .code(),
            None
        );
    }

    /// AT-SPI 变体（upgrade.md §7.3/§7.4，P2）：Display 文案、code 映射与
    /// source 链锁定（新契约字面量，防后续无意识漂移）。
    #[test]
    fn atspi_variants_display_code_and_source_chain() {
        let io_err = std::io::Error::other("a11y bus not found");
        let expected = format!("AT-SPI 连接失败（a11y 总线不可达或订阅失败）: {io_err}");
        let err = Error::AtspiConnectFailed(Box::new(io_err));
        assert_eq!(err.to_string(), expected);
        assert_eq!(err.code(), Some("atspi-connect-failed"));
        assert!(std::error::Error::source(&err).is_some(), "源错误保留在链上");

        assert_eq!(
            Error::AtspiEditUnsupported.to_string(),
            "控件不支持 EditableText（无法直写值）"
        );
        assert_eq!(Error::AtspiEditUnsupported.code(), None, "实现细节级无机器码");
        let io_err = std::io::Error::other("permission denied");
        let expected = format!("EditableText 直写失败: {io_err}");
        let err = Error::AtspiEditFailed(Box::new(io_err));
        assert_eq!(err.to_string(), expected);
        assert_eq!(Error::AtspiEditFailed(Box::new(std::io::Error::other("x"))).code(), None);
        assert!(std::error::Error::source(&err)
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .is_some());
    }
}
