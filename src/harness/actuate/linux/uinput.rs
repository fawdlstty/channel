//! uinput 注入路线（feature `linux-uinput`；upgrade.md §7.1，**本期为文档化
//! 占位 + 诚实错误**）。
//!
//! ## 双闸（§7.1，语义完整保留）
//!
//! 1. 编译期：feature `linux-uinput`（显式 opt-in，非 default）；
//! 2. 运行时：环境变量 `HARNESS_ALLOW_UINPUT=1` 且 `/dev/uinput` 可写
//!    （[`uinput_gates_open`]）——内核级注入面大（可绕过应用级输入锁），
//!    默认关。
//!
//! ## 为什么本期不实现（诚实降级的工程理由）
//!
//! uinput 是**相对输入流**（EV_REL 无绝对位置、EV_ABS 需要与合成器一致
//! 的屏幕范围语义）——「绝对坐标 click(x,y)」要么持续追踪指针位置（ydotool
//! 以常驻 daemon 维持该状态），要么按 Wayland 下不可知的屏幕几何构造 ABS
//! 设备；一次性进程模型（harness exec/serve 的调用形态）两者都拿不到。
//! 盲写相对事件会产生不可预测的位移——比不做更糟。因此：
//!
//! - 双闸全开时 [`super::platform_actuator_linux`] 返回 [`UinputActuator`]，
//!   各原语返回 [`Error::UinputNotImplemented`]（错误文案指向 ydotool 与
//!   libei+RemoteDesktop portal 路线）——比静默 None 更诚实：opt-in 已被
//!   识别，是能力缺席而非未授权；
//! - 真正的 Wayland 原生注入走 libei + RemoteDesktop portal（远期，
//!   `linux-portal` 承载，§7.5-3 风险已登记）。

use crate::harness::actuate::{Actuator, MouseButton};
use crate::harness::Error;

/// 双闸判定（纯函数，单测锁定）：`HARNESS_ALLOW_UINPUT` 恰为 `1`（空白
/// 容忍）且 `/dev/uinput` 可写（传参化，避免单测依赖真机设备）。
pub fn uinput_gates_open(env_flag: Option<String>, device_writable: bool) -> bool {
    matches!(env_flag.as_deref(), Some(value) if value.trim() == "1") && device_writable
}

/// `/dev/uinput` 可写探测（open 写句柄即返回——无 ioctl 前 open 无副作用；
/// 权限不足/设备缺席 → false）。
pub fn device_writable() -> bool {
    std::fs::OpenOptions::new().write(true).open("/dev/uinput").is_ok()
}

/// uinput 占位执行器：全部原语诚实报错（见模块文档——本期不盲写相对输入）。
#[derive(Debug, Clone, Copy, Default)]
pub struct UinputActuator;

impl Actuator for UinputActuator {
    fn click(&self, _x: f64, _y: f64, _button: MouseButton, _clicks: u32) -> Result<(), Error> {
        Err(Error::UinputNotImplemented)
    }

    fn drag(
        &self,
        _from_x: f64,
        _from_y: f64,
        _to_x: f64,
        _to_y: f64,
        _via: &[(f64, f64)],
        _duration_ms: u64,
        _steps: u32,
    ) -> Result<(), Error> {
        Err(Error::UinputNotImplemented)
    }

    fn type_text(&self, _text: &str, _interval_ms: u64) -> Result<(), Error> {
        Err(Error::UinputNotImplemented)
    }

    fn key_combo(&self, _combo: &str) -> Result<(), Error> {
        Err(Error::UinputNotImplemented)
    }

    fn set_value_at(&self, _x: f64, _y: f64, _text: &str) -> Result<(), Error> {
        Err(Error::UinputNotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 双闸：flag 恰为 1（空白容忍）且设备可写；缺一即关。
    #[test]
    fn gates_require_exact_flag_and_writable_device() {
        assert!(uinput_gates_open(Some("1".into()), true));
        assert!(uinput_gates_open(Some(" 1 ".into()), true), "容忍空白");
        assert!(!uinput_gates_open(Some("0".into()), true));
        assert!(!uinput_gates_open(Some("yes".into()), true));
        assert!(!uinput_gates_open(None, true), "未设置 → 关");
        assert!(!uinput_gates_open(Some("1".into()), false), "设备不可写 → 关");
    }

    /// 占位执行器：全部原语诚实失败（错误码 uinput-unavailable 供宿主分支）。
    #[test]
    fn placeholder_actuator_fails_honestly() {
        let actuator = UinputActuator;
        for result in [
            actuator.click(1.0, 1.0, MouseButton::Left, 1),
            actuator.drag(0.0, 0.0, 1.0, 1.0, &[], 10, 4),
            actuator.type_text("x", 0),
            actuator.key_combo("ctrl+s"),
            actuator.set_value_at(1.0, 1.0, "v"),
        ] {
            let error = result.unwrap_err();
            assert_eq!(error.code(), Some("uinput-unavailable"), "{error}");
            assert!(error.to_string().contains("未实现"), "{error}");
        }
    }
}
