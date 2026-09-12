//! Linux 平台注入接线层（upgrade.md §7.1/§7.4；linux.md v1 收编）。
//!
//! 与 [`crate::harness::snapshot::linux`] 对称：平台执行器入口
//! [`platform_actuator_linux`] + 全量编译的子模块：
//!
//! | 子模块 | 内容 | 状态 |
//! |---|---|---|
//! | [`xtest`] | X11 XTest 键鼠注入（xdotool 同款底层） | P1 实现 |
//! | [`clipboard`] | xclip/wl-copy 运行时探测 + 粘贴骨架（平台无关善后协议） | P1 实现 |
//! | [`uinput`] | uinput 注入路线（ydotool 同款；显式 opt-in） | 占位 + 诚实错误（见其模块文档） |
//!
//! 实际走哪条路由**运行环境探测**决定（无按子 feature 的编译期裁剪）。
//! 选路（§7.3 能力矩阵）：X11 会话（`DISPLAY` 在场）优先
//! [`xtest::XTestActuator`]；初始化失败（连接/XTEST 扩展缺席）→ warn
//! 日志 + `None`（bin 提示「本平台无操作类执行器」自动降级只读，截屏等
//! 只读能力不受影响）。纯 Wayland 且 uinput 双闸全开 → [`uinput::UinputActuator`]
//! （本期占位：逐原语诚实报错）。Wayland 原生注入（libei + RemoteDesktop
//! portal）远期评估，见 §7.1/§13。

use std::sync::Arc;

use crate::harness::actuate::Actuator;

/// 子模块：X11 XTest 注入。
pub mod xtest;
/// 子模块：剪贴板粘贴（xclip/wl-copy 运行时探测 + 平台无关善后骨架；
/// X11 与 Wayland 均可用——§7.4 模块表原案）。
pub mod clipboard;
/// 子模块：uinput 注入路线（显式 opt-in；另需 `HARNESS_ALLOW_UINPUT=1`）。
pub mod uinput;

/// Linux 注入执行器入口（actuate::platform_actuator 的非 Windows 分发目标）。
///
/// 选路见模块文档；返回 `None` = 操作类工具面不可用（bin 自动降级只读）。
/// 探测复用 [`crate::harness::snapshot::linux::detect_backends`]（环境判定
/// 单一来源；真正的连接/扩展校验在执行器构造内做）。
pub fn platform_actuator_linux() -> Option<Arc<dyn Actuator>> {
    let backends = crate::harness::snapshot::linux::detect_backends();
    // X11 会话（含 XWayland/WSLg/VcXsrv）→ XTest。
    if backends.x11 {
        match xtest::XTestActuator::new() {
            Ok(actuator) => return Some(Arc::new(actuator)),
            // 初始化失败不静默切 uinput（输入语义不同）：诚实 None，只读面
            // 不受影响（错误细节进 warn 日志供排障）。
            Err(error) => {
                tracing::warn!(error = %error, "XTest 执行器初始化失败，操作类工具面降级为不可用");
                return None;
            }
        }
    }
    // 纯 Wayland 下的受限替代：uinput 双闸（HARNESS_ALLOW_UINPUT=1 +
    // /dev/uinput 可写）。
    if backends.uinput && uinput::uinput_gates_open(
        std::env::var("HARNESS_ALLOW_UINPUT").ok(),
        uinput::device_writable(),
    ) {
        tracing::warn!("uinput 注入路线已 opt-in（本期占位实现，操作原语将诚实报错）");
        return Some(Arc::new(uinput::UinputActuator));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 选路不 panic 且与探测一致（X11 在场的真实连接/注入行为由 xtest.rs
    /// 的 #[ignore] 实机测试覆盖；本测试只锁「接线层永不意外暴露执行器
    /// 之外的语义」）。
    #[test]
    fn actuator_entry_never_panics() {
        let actuator = platform_actuator_linux();
        if crate::harness::snapshot::linux::detect_backends().x11 {
            // 本机有 X 会话（如 :1）时 XTest 构造应成功（扩展普遍在场）；
            // 失败也接受（如 server 无 XTEST），只要不 panic。
            let _ = actuator.map(|a| a.key_combo("no-such-combo"));
        } else {
            // 无 X 会话且 uinput 未开 → None（默认 feature 集下）。
            if std::env::var("HARNESS_ALLOW_UINPUT").map(|v| v.trim() == "1") != Ok(true) {
                assert!(actuator.is_none());
            }
        }
    }
}
