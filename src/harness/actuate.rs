//! 操作类原子工具（design.md §22.4 H3 v1.11，用户要求 Q——§22.8-3 方案乙）。
//!
//! 键鼠模拟五原语，供 Python 运行时 `ctx.ui`（§10.2）回放与调试使用：
//!
//! | 原语 | 语义 | Windows 实现 |
//! |---|---|---|
//! | `click` | 屏幕坐标点击（左/右键、单击/双击） | `SetCursorPos` + `SendInput` 按下/抬起 |
//! | `drag` | 按下 → 经可选 via 途经点采样移动 → 抬起（等价 §6.3 录制侧聚合的一等手势） | 逐采样点 `SetCursorPos` |
//! | `type_text` | 键入文本（UNICODE 注入，中文可用；`\n` → 回车） | `KEYEVENTF_UNICODE` 逐 UTF-16 单元 |
//! | `key` | 组合键/快捷键（`ctrl+s`、`ctrl+shift+n`…） | VK 序列按下/抬起（修饰键包裹） |
//! | `set_value` | 坐标处控件 ValuePattern 直写 | UIA `SetValue`（[`snapshot::uia`]） |
//!
//! # 授权门（§22.7）
//!
//! 工具面分层授权：**只读感知默认可用；操作类仅当进程显式启动**——bin 侧
//! `--actuation` 或环境变量 `HARNESS_ACTUATION=1`，注册表持有
//! [`Actuator`] 才暴露五原语；未授权进程调用一律 `ok:false`，错误文案
//! 指明授权开关（单测锁定）。
//!
//! 实现须知与 [`snapshot::ControlProbe`] 同规矩：可能阻塞（SendInput 间隔
//! 睡眠 / UIA 无强制超时），调用方放入阻塞线程并外加超时。

use std::sync::Arc;

/// Linux 平台接线层（upgrade.md §7.4：XTest/uinput/clipboard 子模块按
/// feature 门控；平台执行器入口 platform_actuator_linux）。
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod sendinput;

/// VK → X11 keysym 静态映射表（跨平台纯逻辑，P1 填充；Windows 目标同样
/// 编译供对拍测试）。
pub mod vk_keysym;

/// 鼠标按键（v1 覆盖左/右键；中键与 X1/X2 录制侧不回放，§6.3 同边界）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

/// 操作原语统一接口（可注入：生产 = [`sendinput::SendInputActuator`]，
/// 测试 = 注册表单测内的记录器）。错误统一为 [`crate::harness::Error`]（P3-13：
/// Display 即 serve wire 文案）。
pub trait Actuator: Send + Sync {
    fn click(&self, x: f64, y: f64, button: MouseButton, clicks: u32) -> crate::harness::Result<()>;
    /// 按下 → 经 `via` 途经点依次平滑采样移动 → 抬起（`via` 为空 = 直线；
    /// 每段采样 `steps` 点；`duration_ms` 为整次拖拽总时长，§10.2）。
    // 扁平参数 = JSON 协议字段（fromX/fromY/toX/toY/via/durationMs/steps）
    // 的直译，特意不加 struct 打包（clippy::too_many_arguments 豁免）。
    #[allow(clippy::too_many_arguments)]
    fn drag(
        &self,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        via: &[(f64, f64)],
        duration_ms: u64,
        steps: u32,
    ) -> crate::harness::Result<()>;
    fn type_text(&self, text: &str, interval_ms: u64) -> crate::harness::Result<()>;
    fn key_combo(&self, combo: &str) -> crate::harness::Result<()>;
    /// 坐标处控件 ValuePattern 直写（不支持 ValuePattern / 无控件 → Err）。
    fn set_value_at(&self, x: f64, y: f64, text: &str) -> crate::harness::Result<()>;
}

/// 生产执行器（Windows = SendInput/UIA；Linux = XTest/uinput
/// （linux::platform_actuator_linux，P1 填充）；其余平台 None → 注册表
/// 拒绝启用操作类，协议层照常可测）。
pub fn platform_actuator() -> Option<Arc<dyn Actuator>> {
    #[cfg(windows)]
    {
        Some(Arc::new(sendinput::SendInputActuator))
    }
    #[cfg(target_os = "linux")]
    {
        linux::platform_actuator_linux()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

/* --- VK 名称表与组合键解析（跨平台纯逻辑，单测锁定） ------------------------- */

/// 修饰键位掩码（[`parse_combo`] 返回值的高层语义）。
pub const MOD_SHIFT: u32 = 1;
pub const MOD_CTRL: u32 = 2;
pub const MOD_ALT: u32 = 4;
pub const MOD_WIN: u32 = 8;

/// Windows 虚拟键码（稳定 ABI 值，跨平台测试底座；与 winuser.h 一致）。
pub const VK_SHIFT: u32 = 0x10;
pub const VK_CONTROL: u32 = 0x11;
pub const VK_MENU: u32 = 0x12;
pub const VK_LWIN: u32 = 0x5B;
pub const VK_RETURN: u32 = 0x0D;

/// 键名 → 虚拟键码（字母/数字取大小写无关主位；F1~F24；常用导航/编辑键；
/// 修饰键也在表内——供 [`parse_combo`] 把「修饰键作主键」化为专用错误）。
pub fn vkey_of_name(name: &str) -> Option<u32> {
    let name = name.trim().to_ascii_lowercase();
    if name.len() == 1 {
        let c = name.chars().next()?;
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(rest) = name.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x6F + n); // VK_F1 = 0x70
            }
        }
        return None;
    }
    Some(match name.as_str() {
        "ctrl" | "control" => VK_CONTROL,
        "alt" => VK_MENU,
        "shift" => VK_SHIFT,
        "win" | "logo" => VK_LWIN,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "space" => 0x20,
        "backspace" | "bs" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        _ => return None,
    })
}

/// 解析组合键（`ctrl+shift+s`）：全部但最后一个 token 必须是修饰键；
/// 主键不能是修饰键（`ctrl+alt` → Err）。返回 `(修饰位掩码, 主键 VK)`。
pub fn parse_combo(combo: &str) -> Result<(u32, u32), crate::harness::Error> {
    let tokens: Vec<&str> = combo.split('+').map(str::trim).filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return Err(crate::harness::Error::ComboEmpty);
    }
    let mut mods = 0u32;
    for token in &tokens[..tokens.len() - 1] {
        mods |= match *token {
            "ctrl" | "control" => MOD_CTRL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" | "logo" => MOD_WIN,
            other => {
                return Err(crate::harness::Error::NotAModifier(other.to_string()));
            }
        };
    }
    let main = tokens[tokens.len() - 1];
    let vk = vkey_of_name(main).ok_or_else(|| crate::harness::Error::UnknownKeyName(main.to_string()))?;
    if vk == VK_SHIFT || vk == VK_CONTROL || vk == VK_MENU || vk == VK_LWIN {
        return Err(crate::harness::Error::ModifierAsMainKey);
    }
    Ok((mods, vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vkey_table_covers_letters_digits_fkeys_and_names() {
        assert_eq!(vkey_of_name("a"), Some(0x41));
        assert_eq!(vkey_of_name("S"), Some(0x53));
        assert_eq!(vkey_of_name("5"), Some(0x35));
        assert_eq!(vkey_of_name("f12"), Some(0x7B));
        assert_eq!(vkey_of_name("f1"), Some(0x70));
        assert_eq!(vkey_of_name("f25"), None);
        assert_eq!(vkey_of_name("enter"), Some(VK_RETURN));
        assert_eq!(vkey_of_name("esc"), Some(0x1B));
        assert_eq!(vkey_of_name("pagedown"), Some(0x22));
        assert_eq!(vkey_of_name("left"), Some(0x25));
        // 修饰键在表内（供 parse_combo 的「主键为修饰键」专用错误分支）。
        assert_eq!(vkey_of_name("shift"), Some(VK_SHIFT));
        assert_eq!(vkey_of_name("alt"), Some(VK_MENU));
        assert_eq!(vkey_of_name("nope"), None);
    }

    #[test]
    fn combo_parse_modifiers_and_main_key() {
        // Error 无 PartialEq（io/json 源错误不可比），Ok 载荷经 unwrap 比对。
        assert_eq!(parse_combo("ctrl+s").unwrap(), (MOD_CTRL, 0x53));
        assert_eq!(parse_combo("ctrl+shift+n").unwrap(), (MOD_CTRL | MOD_SHIFT, 0x4E));
        assert_eq!(parse_combo("ENTER").unwrap(), (0, VK_RETURN));
        assert_eq!(parse_combo(" alt + f4 ").unwrap(), (MOD_ALT, 0x73));
        // 主键是修饰键 → Err；非修饰键 token 出现在中间 → Err；空 → Err。
        assert!(parse_combo("ctrl+alt").is_err());
        assert!(parse_combo("ctrl+nope+s").is_err());
        assert!(parse_combo("").is_err());
    }
}
