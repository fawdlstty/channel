//! Windows VK → X11 keysym 静态映射表（upgrade.md §7.1；跨平台**纯逻辑**）。
//!
//! 服务 Linux XTest 注入：`key` op 的组合键解析产物（[`super::parse_combo`]
//! → [`super::vkey_of_name`]）是 Windows VK 码（稳定 ABI 值），X11 侧需要
//! keysym 才能查 keycode 注入——本表即两者之间的桥。覆盖 [`super::vkey_of_name`]
//! 的**全部键名空间**（字母/数字/F1~F24/导航编辑键/修饰键），保证凡
//! `parse_combo` 接受的组合键在 X11 侧都有 keysym 可查，不会因表缺项失败。
//!
//! 键码对照（X keysym 值来自 X11 `<Xkeysymdef.h>`，跨实现稳定）：
//!
//! | 键类 | VK | keysym |
//! |---|---|---|
//! | 字母 A~Z | 0x41~0x5A | 0x61~0x7A（`XK_a`~`XK_z`；键盘主列即小写，大写靠 Shift 列） |
//! | 数字 0~9 | 0x30~0x39 | 同码位（Latin-1 恒等） |
//! | F1~F24 | 0x70~0x87 | 0xFFBE~0xFFD5（`XK_F1`=0xFFBE，等差 +1） |
//! | 命名键（回车/Tab/方向/编辑…） | 见 [`NAMED_VK_KEYSYM`] | 各 `XK_` 值 |
//!
//! 坐标/键盘语义注记：VK 与 keysym 都面向**物理键**（无大小写状态）；
//! 屏幕坐标的物理像素/CSS 坐标差异见 `snapshot::linux::x11_screen` 模块注记。

/// XK_Shift_L（左 Shift 修饰键）。
pub const XK_SHIFT_L: u32 = 0xFFE1;
/// XK_Control_L（左 Ctrl 修饰键）。
pub const XK_CONTROL_L: u32 = 0xFFE3;
/// XK_Alt_L（左 Alt 修饰键）。
pub const XK_ALT_L: u32 = 0xFFE9;
/// XK_Super_L（左 Super/Win/Meta 修饰键）。
pub const XK_SUPER_L: u32 = 0xFFEB;
/// XK_Return（回车；注意不是 ASCII 0x0D）。
pub const XK_RETURN: u32 = 0xFF0D;
/// XK_Tab（注意不是 ASCII 0x09）。
pub const XK_TAB: u32 = 0xFF09;
/// XK_BackSpace。
pub const XK_BACKSPACE: u32 = 0xFF08;
/// XK_Escape。
pub const XK_ESCAPE: u32 = 0xFF1B;
/// XK_space（Latin-1 区，恰与 ASCII 码位恒等）。
pub const XK_SPACE: u32 = 0x0020;

/// 命名键（字母/数字/F 键之外）的 VK → keysym 静态表（约 20 项，含修饰键
/// ——`key` op 不直接打修饰键，但键入路径的 Shift 包裹与错误分支需要它们）。
const NAMED_VK_KEYSYM: &[(u32, u32)] = &[
    // 修饰键（VK 常量见 super；keysym 取各左键，XTest 按 keycode 注入时
    // 左右修饰键等效于该修饰逻辑位）。
    (super::VK_SHIFT, XK_SHIFT_L),
    (super::VK_CONTROL, XK_CONTROL_L),
    (super::VK_MENU, XK_ALT_L),
    (super::VK_LWIN, XK_SUPER_L),
    // 编辑/导航键（VK 码 = winuser.h；keysym = Xkeysymdef.h）。
    (0x08, XK_BACKSPACE),      // BackSpace
    (0x09, XK_TAB),            // Tab
    (0x0D, XK_RETURN),         // Enter/Return
    (0x1B, XK_ESCAPE),         // Esc
    (0x20, XK_SPACE),          // Space
    (0x21, 0xFF55),            // PageUp = XK_Prior
    (0x22, 0xFF56),            // PageDown = XK_Next
    (0x23, 0xFF57),            // End
    (0x24, 0xFF50),            // Home
    (0x25, 0xFF51),            // Left
    (0x26, 0xFF52),            // Up
    (0x27, 0xFF53),            // Right
    (0x28, 0xFF54),            // Down
    (0x2D, 0xFF63),            // Insert
    (0x2E, 0xFFFF),            // Delete
];

/// VK → keysym（纯查表；字母/数字/F 键为连续码段，直接算术映射）。
/// 不在 [`super::vkey_of_name`] 键名空间内的 VK（如 OEM 标点 `0xBA`~）→
/// `None`——组合键面本来就不产生这些 VK。
pub fn keysym_of_vk(vk: u32) -> Option<u32> {
    match vk {
        // 字母：VK 大写码位 → XK 小写（X 键盘主列 = 小写，大写由 Shift 列表达）。
        0x41..=0x5A => Some(vk + 0x20),
        // 数字：Latin-1 keysym 与 ASCII 码位恒等。
        0x30..=0x39 => Some(vk),
        // F1~F24（VK_Fn = 0x6F + n；XK_Fn = 0xFFBE + (n-1) = 0xFFBE +
        // (vk - 0x70)，两套均为连续段）。
        0x70..=0x87 => Some(0xFFBE + (vk - 0x70)),
        _ => NAMED_VK_KEYSYM.iter().find(|(v, _)| *v == vk).map(|(_, k)| *k),
    }
}

/// keysym 的 X 名称（错误文案用；仅表内键可命名）。
pub fn keysym_name(keysym: u32) -> Option<&'static str> {
    Some(match keysym {
        XK_SHIFT_L => "Shift_L",
        XK_CONTROL_L => "Control_L",
        XK_ALT_L => "Alt_L",
        XK_SUPER_L => "Super_L",
        XK_RETURN => "Return",
        XK_TAB => "Tab",
        XK_BACKSPACE => "BackSpace",
        XK_ESCAPE => "Escape",
        XK_SPACE => "space",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{parse_combo, vkey_of_name};
    use super::*;

    /// 修饰键：每键一条断言（VK 常量 → 对应 XK 修饰键）。
    #[test]
    fn modifier_keys_map_to_xk_left_modifiers() {
        assert_eq!(keysym_of_vk(super::super::VK_SHIFT), Some(XK_SHIFT_L));
        assert_eq!(keysym_of_vk(super::super::VK_CONTROL), Some(XK_CONTROL_L));
        assert_eq!(keysym_of_vk(super::super::VK_MENU), Some(XK_ALT_L));
        assert_eq!(keysym_of_vk(super::super::VK_LWIN), Some(XK_SUPER_L));
    }

    /// 编辑/导航键：每键一条断言（VK → XK 值逐一锁定，防手抄漂移）。
    #[test]
    fn named_keys_map_one_to_one() {
        assert_eq!(keysym_of_vk(0x08), Some(0xFF08), "BackSpace");
        assert_eq!(keysym_of_vk(0x09), Some(0xFF09), "Tab");
        assert_eq!(keysym_of_vk(0x0D), Some(0xFF0D), "Return");
        assert_eq!(keysym_of_vk(0x1B), Some(0xFF1B), "Escape");
        assert_eq!(keysym_of_vk(0x20), Some(0x0020), "space");
        assert_eq!(keysym_of_vk(0x21), Some(0xFF55), "PageUp=Prior");
        assert_eq!(keysym_of_vk(0x22), Some(0xFF56), "PageDown=Next");
        assert_eq!(keysym_of_vk(0x23), Some(0xFF57), "End");
        assert_eq!(keysym_of_vk(0x24), Some(0xFF50), "Home");
        assert_eq!(keysym_of_vk(0x25), Some(0xFF51), "Left");
        assert_eq!(keysym_of_vk(0x26), Some(0xFF52), "Up");
        assert_eq!(keysym_of_vk(0x27), Some(0xFF53), "Right");
        assert_eq!(keysym_of_vk(0x28), Some(0xFF54), "Down");
        assert_eq!(keysym_of_vk(0x2D), Some(0xFF63), "Insert");
        assert_eq!(keysym_of_vk(0x2E), Some(0xFFFF), "Delete");
    }

    /// 字母：VK 大写 → XK 小写（两端点 + 抽样；'a' 主列语义）。
    #[test]
    fn letters_map_uppercase_vk_to_lowercase_keysym() {
        assert_eq!(keysym_of_vk(0x41), Some(0x61), "A → XK_a");
        assert_eq!(keysym_of_vk(0x53), Some(0x73), "S → XK_s");
        assert_eq!(keysym_of_vk(0x5A), Some(0x7A), "Z → XK_z");
    }

    /// 数字：Latin-1 keysym 与 ASCII 码位恒等（两端点）。
    #[test]
    fn digits_map_identity() {
        assert_eq!(keysym_of_vk(0x30), Some(0x30), "0 → XK_0");
        assert_eq!(keysym_of_vk(0x39), Some(0x39), "9 → XK_9");
    }

    /// F 键：F1/F2/F12/F24 端点与常量对照（XK_F1=0xFFBE）。
    #[test]
    fn function_keys_map_xk_f_series() {
        assert_eq!(keysym_of_vk(0x70), Some(0xFFBE), "F1");
        assert_eq!(keysym_of_vk(0x71), Some(0xFFBF), "F2");
        assert_eq!(keysym_of_vk(0x7B), Some(0xFFC9), "F12");
        assert_eq!(keysym_of_vk(0x87), Some(0xFFD5), "F24");
    }

    /// 键名空间外的 VK → None（OEM 标点/未定义码位，组合键面不产生）。
    #[test]
    fn unknown_vks_return_none() {
        assert_eq!(keysym_of_vk(0x00), None);
        assert_eq!(keysym_of_vk(0x0A), None, "LF 无 VK 键名");
        assert_eq!(keysym_of_vk(0xBA), None, "OEM 分号");
        assert_eq!(keysym_of_vk(0xFF), None);
    }

    /// 完备性（关键契约）：凡 `vkey_of_name` 认识的键名，keysym 必有映射
    /// ——`key` op 在 X11 侧不因表缺项失败。逐名字扫描（含别名）。
    #[test]
    fn every_parsable_key_name_has_a_keysym() {
        let names: Vec<String> = ('a'..='z')
            .chain('A'..='Z')
            .chain('0'..='9')
            .map(|c| c.to_string())
            .chain((1..=24).map(|n| format!("f{n}")))
            .chain(
                [
                    "ctrl", "control", "alt", "shift", "win", "logo", "enter", "return", "tab",
                    "esc", "escape", "space", "backspace", "bs", "delete", "del", "insert",
                    "ins", "home", "end", "pageup", "pgup", "pagedown", "pgdn", "up", "down",
                    "left", "right",
                ]
                .iter()
                .map(ToString::to_string),
            )
            .collect();
        assert!(names.len() >= 40, "键名空间至少 40 项: {}", names.len());
        for name in names {
            let vk = vkey_of_name(&name)
                .unwrap_or_else(|| panic!("vkey_of_name({name}) 应在表内"));
            assert!(
                keysym_of_vk(vk).is_some(),
                "keysym 缺项：{name}（VK 0x{vk:X}）"
            );
        }
    }

    /// 组合键端到端（纯逻辑段）：parse_combo 的产物 VK 经本表必有 keysym。
    #[test]
    fn parse_combo_products_flow_into_keysym_table() {
        let (_, vk) = parse_combo("ctrl+shift+n").unwrap();
        assert_eq!(keysym_of_vk(vk), Some(0x6E));
        let (_, vk) = parse_combo("alt+f4").unwrap();
        assert_eq!(keysym_of_vk(vk), Some(0xFFC1), "F4");
        let (_, vk) = parse_combo("ENTER").unwrap();
        assert_eq!(keysym_of_vk(vk), Some(XK_RETURN));
    }

    /// keysym_name（错误文案辅助）：表内可命名、表外 None。
    #[test]
    fn keysym_name_covers_named_constants_only() {
        assert_eq!(keysym_name(XK_CONTROL_L), Some("Control_L"));
        assert_eq!(keysym_name(XK_RETURN), Some("Return"));
        assert_eq!(keysym_name(0x61), None, "XK_a 不在命名清单");
    }
}
