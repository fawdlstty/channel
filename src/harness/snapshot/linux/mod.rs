//! Linux 平台感知接线层（upgrade.md §7.3/§7.4；linux.md v1 收编）。
//!
//! 职责：环境探测（[`detect_backends`]）+ 感知/截屏的平台入口转发
//! （[`platform_probe_linux`] / [`capture_screen_linux`]）。子模块随
//! `harness` feature 全量编译（§11.3；不再按子 feature 裁剪）：
//!
//! | 子模块 | 内容 | 状态 |
//! |---|---|---|
//! | [`x11_screen`] | X11 GetImage 截屏（x11rb + png） | P1 实现 |
//! | [`atspi_probe`] | AT-SPI2 `ControlProbe` 实现（zbus） | P2（并行开发） |
//! | [`portal_screen`] | Wayland xdg-desktop-portal 截屏 | P3 实现 |
//!
//! 实际走哪条路由**运行环境探测**决定（[`detect_backends`] + 入口内
//! 选路），编译期不再二选一。
//!
//! 截屏选路（[`capture_screen_linux`]）：X11（含 XWayland/WSLg/VcXsrv，
//! `DISPLAY` 在场）优先 x11rb；纯 Wayland 走 portal（需授权，失败报
//! [`Error::WaylandPortalUnavailable`]）；均无显示服务报
//! [`Error::NoDisplayServer`]——version op 的 `snapshotAvailable` 与感知类
//! op 的 `ok:false` 降级路径由此驱动，serve 协议照常可用（纯 TTY headless
//! 回放宿主语义，§7.2-2）。
//!
//! 感知入口 [`platform_probe_linux`] 在 a11y 总线不可达时 warn + `None`
//! 诚实降级（与 Windows UIA 初始化失败同语义）。
//!
//! 注入侧接线（XTest/uinput）在 [`crate::harness::actuate::linux`]，与本模块对称。

use std::sync::Arc;

use serde::Serialize;
use futures_lite::future::block_on;

use crate::harness::snapshot::screen::ScreenCapture;
use crate::harness::snapshot::ControlProbe;
use crate::harness::Error;

/// 子模块：X11 截屏（P1 实现：x11rb GetImage + png）。
pub mod x11_screen;
/// 子模块：AT-SPI2 控件树感知（P2 填 AtspiProbe）。
pub mod atspi_probe;
/// 子模块：Wayland portal 截屏（P3 实现：ashpd）。
pub mod portal_screen;

/// Linux 后端能力探测结果（upgrade.md §7.3 能力矩阵的机器可读形态；
/// version op 的 `backends` 扩展与 app 侧采集面判定共用）。
///
/// 语义：**粗探测**——「环境存在」即可用候选（后端实现已随 `harness`
/// 全量编译，不再有"未编译"项），不代表运行时一定成功（portal 授权、
/// AT-SPI 总线实际可达性等在真实调用时才揭晓，失败按各自错误码降级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Backends {
    /// AT-SPI2 感知可用（D-Bus 会话总线/a11y 总线线索在场；实际总线
    /// 连接由 P2 细化）。
    pub atspi: bool,
    /// X11 服务可用（`DISPLAY` 在场；含 XWayland/WSLg/VcXsrv）。
    pub x11: bool,
    /// Wayland 合成器会话在场（`WAYLAND_DISPLAY`；纯显示服务线索，截屏
    /// 还需 portal、注入还受限）。
    pub wayland: bool,
    /// xdg-desktop-portal 可用候选（`XDG_RUNTIME_DIR` 在场且目录存在——
    /// portal 全家桶都挂在会话运行时目录上）。
    pub portal: bool,
    /// uinput 注入可用候选（`/dev/uinput` 存在；运行时另需
    /// `HARNESS_ALLOW_UINPUT=1` 显式 opt-in）。
    pub uinput: bool,
}

/// 环境变量值 → 在场判定（`DISPLAY=` 空白值等同未设置；纯逻辑供单测）。
fn value_present(value: Option<String>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// 环境变量在场判定（探测辅助；空白值等同未设置）。
fn env_present(key: &str) -> bool {
    value_present(std::env::var(key).ok())
}

/// `XDG_RUNTIME_DIR` 在场且真实存在目录（portal 判定细化：portal 全家桶
/// 的 D-Bus socket 都挂在该目录，变量在而目录缺（SSH 裸环境变量泄漏等）
/// 时 portal 必不可达，提前诚实排除）。
fn runtime_dir_present() -> bool {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .and_then(|dir| (!dir.trim().is_empty()).then(|| std::path::Path::new(&dir).is_dir()))
        .unwrap_or(false)
}

/// AT-SPI 总线可达性探测（P2 细化；真实 D-Bus 查询，零副作用——不做
/// activation，不拉起 at-spi）：
///
/// 1. 会话总线线索（`DBUS_SESSION_BUS_ADDRESS` 或 `XDG_RUNTIME_DIR`）缺席
///    → false（短路，免连）；
/// 2. 会话总线上 `org.a11y.Bus` 已在场（有 AT/合成器拉起过）**或**在
///    activatable 清单（服务文件在，D-Bus activation 可即时拉起）→ true。
///
/// 仍属「可用候选」语义：实际建连/树内容的成败由 [`AtspiProbe::new`]
/// 真实调用时揭晓（失败按 `atspi-connect-failed` 降级）。
fn atspi_bus_available() -> bool {
    if !(env_present("DBUS_SESSION_BUS_ADDRESS") || env_present("XDG_RUNTIME_DIR")) {
        return false;
    }
    block_on(async {
        let Ok(bus) = atspi::zbus::Connection::session().await else {
            return false;
        };
        let Ok(dbus) = atspi::zbus::fdo::DBusProxy::new(&bus).await else {
            return false;
        };
        let a11y_name = atspi::zbus::names::WellKnownName::from_static_str("org.a11y.Bus")
            .expect("org.a11y.Bus 是合法 well-known 名");
        let owned = dbus.name_has_owner(a11y_name.clone().into()).await.unwrap_or(false);
        let activatable = dbus
            .list_activatable_names()
            .await
            .map(|names| names.iter().any(|name| name.as_str() == "org.a11y.Bus"))
            .unwrap_or(false);
        owned || activatable
    })
}

/// Linux 后端能力探测（upgrade.md §7.3；P0 地基粗判 + P2 的 atspi 细化）：
///
/// - `x11`：`DISPLAY` 非空；
/// - `wayland`：`WAYLAND_DISPLAY` 非空（显示服务线索同时用于错误指引，
///   纯 Wayland 下截屏错误码要能指向 portal）；
/// - `portal`：`XDG_RUNTIME_DIR` 非空且目录存在（细化自 P0 的
///   「仅变量非空」——见 [`runtime_dir_present`]）；
/// - `atspi`：[`atspi_bus_available`]（会话总线上 org.a11y.Bus 在场或可
///   激活——P2 起为真实 D-Bus 查询，非纯 env 粗判）；
/// - `uinput`：`/dev/uinput` 存在。
///
/// 除 atspi 外纯环境/文件系统探测零外部调用；atspi 项一次本机会话总线
/// 连接 + 两个 fdo 查询（无 activation 副作用）——headless CI（无会话
/// 总线）下诚实为 false，不误报。
pub fn detect_backends() -> Backends {
    Backends {
        atspi: atspi_bus_available(),
        x11: env_present("DISPLAY"),
        wayland: env_present("WAYLAND_DISPLAY"),
        portal: runtime_dir_present(),
        uinput: std::path::Path::new("/dev/uinput").exists(),
    }
}

/// Linux 感知探测入口（snapshot::platform_probe 的非 Windows 分发目标）。
///
/// 返回 [`atspi_probe::AtspiProbe`]（a11y 总线不可达 → warn + None 诚实
/// 降级，与 Windows UIA 初始化失败同语义）。
pub fn platform_probe_linux() -> Option<Arc<dyn ControlProbe>> {
    match atspi_probe::AtspiProbe::new() {
        Ok(probe) => Some(Arc::new(probe)),
        Err(e) => {
            tracing::warn!(error = %e, "AT-SPI probe 初始化失败，感知降级不可用");
            None
        }
    }
}

/// Linux 截屏入口（snapshot::screen 的非 Windows 分发目标）。
///
/// 参数与 [`crate::harness::snapshot::screen`] 的区域入口同语义（屏幕坐标矩形，
/// **物理像素**）；返回 [`ScreenCapture`]（PNG 字节 + 尺寸，不落盘）。
/// 选路（§7.1）：
///
/// - `DISPLAY` 在场（X11 原生/XWayland/WSLg/VcXsrv）→ [`x11_screen`]
///   （连接失败报 [`Error::X11ConnectFailed`]，区域与根窗口钳制/空交
///   报错对齐 Windows 语义）；
/// - 纯 Wayland（`WAYLAND_DISPLAY` 在场无 `DISPLAY`）→ [`portal_screen`]
///   （需授权；区域参数不适用——portal 只出整屏）；
/// - 均无显示服务 → [`Error::NoDisplayServer`]（headless 回放宿主语义）。
///
/// 整屏哨兵：`width <= 0 || height <= 0`（如整屏入口的 `(0,0,0,0)`）由
/// 实现层按根窗口几何自查（screen.rs 注记的「矩形未探测」语义）。
pub fn capture_screen_linux(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<ScreenCapture, Error> {
    let wayland = env_present("WAYLAND_DISPLAY");
    if env_present("DISPLAY") {
        return x11_screen::capture_screen(x, y, width, height);
    }
    if wayland {
        return portal_screen::capture_screen_via_portal();
    }
    Err(Error::NoDisplayServer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// detect_backends 与粗判规则逐项一致（环境依赖项按同一 env 重放——
    /// 锁定语义而非锁死某台机器的环境快照）。
    #[test]
    fn detect_backends_follows_env() {
        let backends = detect_backends();
        // atspi 项 P2 起为真实 D-Bus 查询（见 atspi_bus_available），单测
        // 只锁「会话总线线索缺席 → 恒 false」与「探测可重放」，真实总线
        // 路径进 tests/atspi_smoke.rs。
        let bus_env_present =
            env_present("DBUS_SESSION_BUS_ADDRESS") || env_present("XDG_RUNTIME_DIR");
        assert_eq!(
            backends.atspi,
            if bus_env_present { atspi_bus_available() } else { false },
            "atspi：线索缺席短路 false；在场时与 D-Bus 查询一致（可重放）"
        );
        assert_eq!(backends.x11, env_present("DISPLAY"));
        assert_eq!(backends.wayland, env_present("WAYLAND_DISPLAY"));
        assert_eq!(backends.portal, runtime_dir_present());
        assert_eq!(
            backends.uinput,
            std::path::Path::new("/dev/uinput").exists()
        );
    }

    /// 在场判定：未设置/空白值按缺席（`DISPLAY=` 空值等同无 X 会话）。
    #[test]
    fn env_presence_treats_blank_as_absent() {
        assert!(value_present(Some("x".into())));
        assert!(!value_present(Some(String::new())));
        assert!(!value_present(Some("   ".into())));
        assert!(!value_present(None));
    }

    /// Backends 序列化（version op 的 `backends` 字段契约，§7.3）：五个
    /// 布尔字段全量呈现、无多余键。
    #[test]
    fn backends_serialize_for_version_op() {
        let value = serde_json::to_value(detect_backends()).expect("纯派生结构序列化必成");
        let expected_keys = ["atspi", "x11", "wayland", "portal", "uinput"];
        assert_eq!(
            value.as_object().map(|map| map.len()),
            Some(expected_keys.len()),
            "字段数恰为 {}（无多余键）: {value}",
            expected_keys.len()
        );
        for key in expected_keys {
            assert!(value.get(key).is_some_and(serde_json::Value::is_boolean), "缺字段 {key}");
        }
        assert_eq!(value["x11"], serde_json::json!(detect_backends().x11));
    }

    /// 感知入口诚实降级：无 a11y 总线环境（线索缺席）→ warn + None
    /// （真实 probe 查询链进 tests/atspi_smoke.rs）。
    #[test]
    fn probe_degrades_honestly_without_atspi() {
        if !env_present("DBUS_SESSION_BUS_ADDRESS") && !env_present("XDG_RUNTIME_DIR") {
            assert!(platform_probe_linux().is_none(), "无总线线索时诚实降级");
        }
    }

    /// 截屏入口按显示服务选路：
    /// - X11 会话（本机 `DISPLAY=:1` 实测）→ 真实 PNG（成功路径细节由
    ///   x11_screen 的真实测试锁定，这里锁「接线确实接到实现」）；
    /// - 纯 Wayland：不在单测触发 portal（授权对话框副作用）——portal
    ///   选路的真实测试 #[ignore] 在 portal_screen；
    /// - 无显示服务（headless CI）→ `no-display-server` 降级。
    #[test]
    fn capture_entry_routes_by_display_service() {
        let x11 = env_present("DISPLAY");
        let wayland = env_present("WAYLAND_DISPLAY");
        if x11 {
            let shot = capture_screen_linux(0, 0, 0, 0).expect("X11 会话 → 真实截屏");
            assert!(shot.png.starts_with(&[0x89, b'P', b'N', b'G']), "PNG 魔数");
            assert!(shot.width > 0 && shot.height > 0);
        } else if !wayland {
            let error = capture_screen_linux(0, 0, 800, 600).unwrap_err();
            assert_eq!(error.code(), Some("no-display-server"), "{error}");
            assert_eq!(error.to_string(), "无显示服务（纯 TTY/无 X11 与 Wayland 会话，截屏不可用）");
        }
        // 纯 Wayland 分支（portal 授权）由 portal_screen 的 #[ignore]
        // 实测保证——单测不弹授权对话框。
    }
}
