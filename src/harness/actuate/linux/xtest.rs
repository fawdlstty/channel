//! X11 XTest 键鼠注入（feature `linux-x11`；upgrade.md §7.1，P1 实现）。
//!
//! xdotool 同款底层（libXtst → XTEST 协议的 FakeInput 请求），纯 Rust 经
//! `x11rb` 直连（无 libXtst/libX11 硬链）。五原语映射：
//!
//! | 原语 | 实现 |
//! |---|---|
//! | `click` | `FakeInput(MotionNotify)` 定位（屏幕号 255 = 当前屏，xdotool 同款）→ ButtonPress/Release（左=1/右=3） |
//! | `drag` | 按下 → 路径采样移动 → 抬起（采样节奏与 sendinput.rs 同语义：waypoints 线性插值、`duration_ms` 为总时长） |
//! | `key_combo` | [`parse_combo`] → VK → keysym（[`crate::harness::actuate::vk_keysym`]）→ keycode（GetKeyboardMapping 快照）→ 修饰键包裹 tap |
//! | `type_text` | ASCII：keysym 逐字符（`\n`=Return、`\t`=Tab、退格/Escape 特判）；非 ASCII（中文等）：剪贴板粘贴（[`super::clipboard`] 善后骨架 + xclip/wl-copy） |
//! | `set_value` | X11 无 UIA 等价直写通道：需 AT-SPI（feature `linux-atspi`）；缺席时诚实报错（§7.3 能力矩阵 X11 无 AT-SPI 行 set_value=✗） |
//!
//! **坐标语义**：全局物理像素（根窗口坐标），与截屏/Windows DPI-aware 对齐
//! ——不是 CSS 逻辑像素。keysym→keycode 查 GetKeyboardMapping 快照（构造时
//! 一次，避免逐键往返）；目标键落在键盘映射 Shift 列（如 `XK_A`）时自动
//! 包一层 Shift 按下/抬起。
//!
//! **测试红线**：XTest 会注入真实键鼠事件（动用户的鼠标键盘！）——真实
//! 会话测试全部 `#[ignore]`，仅在一次性虚拟桌面（Xvfb）或专用测试机上手动
//! 运行；单测只覆盖纯逻辑（keysym 查表/路径采样/字符分类）。

use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

use crate::harness::actuate::linux::clipboard::{self, CommandClipboardIo};
use crate::harness::actuate::vk_keysym::{
    keysym_of_vk, keysym_name, XK_ALT_L, XK_BACKSPACE, XK_CONTROL_L, XK_ESCAPE, XK_RETURN,
    XK_SHIFT_L, XK_SUPER_L, XK_TAB,
};
use crate::harness::actuate::{parse_combo, Actuator, MouseButton, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use crate::harness::Error;

/// 单次点击按下/抬起之间的停顿（sendinput.rs 同值：目标应用识别完整点击
/// 的最小时序）。
const CLICK_PAUSE_MS: u64 = 15;
/// 组合键各步之间的停顿（sendinput.rs 同值）。
const KEY_PAUSE_MS: u64 = 10;
/// 剪贴板置入后、粘贴前的等待（sendinput.rs 同值起点；VcXsrv 等远程剪贴板
/// 同步竞态下经 [`XTestActuator::type_text_via_clipboard`] 可调大——§7.5-5）。
const CLIPBOARD_SETTLE_MS: u64 = 120;
/// 粘贴后、恢复用户剪贴板前的等待（同上）。
const CLIPBOARD_CONSUME_MS: u64 = 250;

/// XTest 伪事件类型（X11 协议事件码）。
const EVENT_KEY_PRESS: u8 = xproto::KEY_PRESS_EVENT;
const EVENT_KEY_RELEASE: u8 = xproto::KEY_RELEASE_EVENT;
const EVENT_BUTTON_PRESS: u8 = xproto::BUTTON_PRESS_EVENT;
const EVENT_BUTTON_RELEASE: u8 = xproto::BUTTON_RELEASE_EVENT;
const EVENT_MOTION_NOTIFY: u8 = xproto::MOTION_NOTIFY_EVENT;

/// Motion 事件的屏幕号（detail=255 = 当前屏；libXtst 的 screen_number=-1
/// 同款语义，xdotool 即此用法——避免多屏索引换算）。
const MOTION_CURRENT_SCREEN: u8 = 0xFF;

/// 生产执行器：构造时连接 X + 校验 XTEST 扩展 + 快照键盘映射；此后逐调用
/// 注入（无状态、线程安全——`RustConnection` 内部同步，Arc 共享单连接）。
pub struct XTestActuator {
    conn: Arc<RustConnection>,
    /// GetKeyboardMapping 快照：`keymap[i]` = keycode `min_keycode + i` 的
    /// keysym 列（列 0 = 本键、列 1 = Shift 列、列 2+ = AltGr 组）。
    keymap: Vec<Vec<u32>>,
    min_keycode: u8,
    /// 根窗口（Motion 事件的坐标归属）。
    root: xproto::Window,
}

impl XTestActuator {
    /// 连接并初始化（`DISPLAY` 已由选路方判定在场）：连接失败 →
    /// [`Error::X11ConnectFailed`]；X server 无 XTEST 扩展 →
    /// [`Error::XTestUnavailable`]（截屏不受影响，仅注入不可用）。
    pub fn new() -> Result<Self, Error> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|error| Error::X11ConnectFailed(error.to_string()))?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen_num].root;
        // XTEST 扩展在场性（FakeInput 自 XTEST 1.0 可用，版本号不约束；
        // extension_information 需要时自动 ListExtensions——失败视同缺席）。
        if conn.extension_information("XTEST").ok().flatten().is_none() {
            return Err(Error::XTestUnavailable);
        }
        xtest::get_version(&*conn, 2, 2)
            .map_err(|error| Error::X11Protocol { op: "XTest.QueryVersion", detail: error.to_string() })?
            .reply()
            .map_err(|error| Error::X11Protocol { op: "XTest.QueryVersion", detail: error.to_string() })?;
        // 键盘映射快照（keysym→keycode 查表底座；一次往返终身使用）。
        let setup = conn.setup();
        let min_keycode = setup.min_keycode;
        let count = setup.max_keycode - min_keycode + 1;
        let reply = conn
            .get_keyboard_mapping(min_keycode, count)
            .map_err(|error| Error::X11Protocol { op: "GetKeyboardMapping", detail: error.to_string() })?
            .reply()
            .map_err(|error| Error::X11Protocol { op: "GetKeyboardMapping", detail: error.to_string() })?;
        let per = reply.keysyms_per_keycode as usize;
        let keymap: Vec<Vec<u32>> =
            if per == 0 { Vec::new() } else { reply.keysyms.chunks(per).map(Vec::from).collect() };
        Ok(Self { conn, keymap, min_keycode, root })
    }

    /// keysym → (keycode, 是否需按 Shift)。键不在映射内 → Err（错误文案带
    /// keysym 名称/值，便于排查非常规布局）。
    fn keycode_of(&self, keysym: u32) -> Result<(u8, bool), Error> {
        keycode_of_in(&self.keymap, self.min_keycode, keysym).ok_or_else(|| Error::X11Protocol {
            op: "keysym→keycode",
            detail: format!(
                "键盘映射无 keysym 0x{keysym:X}（{}，非常规布局？）",
                keysym_name(keysym).unwrap_or("未命名键")
            ),
        })
    }

    /// 发一条 FakeInput 并 flush（同连接内请求序即事件序；time=0 用客户端
    /// sleep 控节奏，与 xdotool 同款）。键/按钮事件不携带 root（协议忽略），
    /// Motion 事件带本屏根窗口。
    fn fake(&self, event_type: u8, detail: u8, root: xproto::Window, x: i16, y: i16) -> Result<(), Error> {
        xtest::fake_input(&*self.conn, event_type, detail, 0, root, x, y, 0)
            .map_err(|error| Error::X11Protocol { op: "XTest.FakeInput", detail: error.to_string() })?;
        self.conn
            .flush()
            .map_err(|error| Error::X11Protocol { op: "flush", detail: error.to_string() })?;
        Ok(())
    }

    /// 同步（flush + 一次往返等 server 处理完已注入事件；GetInputFocus 是
    /// libX11 XSync 的同款惯用法）。每个原语收尾调用一次——保证「click
    /// 返回后事件已落地」，语义上等价 sendinput 的同步注入。
    fn sync(&self) -> Result<(), Error> {
        self.conn
            .get_input_focus()
            .map_err(|error| Error::X11Protocol { op: "GetInputFocus(sync)", detail: error.to_string() })?
            .reply()
            .map_err(|error| Error::X11Protocol { op: "GetInputFocus(sync)", detail: error.to_string() })?;
        Ok(())
    }

    fn key_event(&self, keycode: u8, press: bool) -> Result<(), Error> {
        self.fake(
            if press { EVENT_KEY_PRESS } else { EVENT_KEY_RELEASE },
            keycode,
            x11rb::NONE,
            0,
            0,
        )
    }

    /// 按下/抬起某 keysym 对应的物理键（修饰键路径；Shift 列语义不适用）。
    fn modifier_event(&self, keysym: u32, press: bool) -> Result<(), Error> {
        let (keycode, _) = self.keycode_of(keysym)?;
        self.key_event(keycode, press)
    }

    /// 伪鼠标移动（全局物理像素；四舍五入 + i16 钳制）。
    fn motion(&self, x: f64, y: f64) -> Result<(), Error> {
        self.fake(EVENT_MOTION_NOTIFY, MOTION_CURRENT_SCREEN, self.root, clamp_i16(x), clamp_i16(y))
    }

    fn button_event(&self, button: u8, press: bool) -> Result<(), Error> {
        self.fake(
            if press { EVENT_BUTTON_PRESS } else { EVENT_BUTTON_RELEASE },
            button,
            x11rb::NONE,
            0,
            0,
        )
    }

    /// keycode tap：按下 → KEY_PAUSE → 抬起 → KEY_PAUSE。
    fn tap_keycode(&self, keycode: u8) -> Result<(), Error> {
        self.key_event(keycode, true)?;
        sleep(Duration::from_millis(KEY_PAUSE_MS));
        self.key_event(keycode, false)?;
        sleep(Duration::from_millis(KEY_PAUSE_MS));
        Ok(())
    }

    /// keysym tap：目标键在键盘映射 Shift 列（如 `XK_A`、`XK_!`）时自动包
    /// Shift 按下/抬起（xdotool 同款语义）。
    fn tap_keysym(&self, keysym: u32) -> Result<(), Error> {
        let (keycode, column_shift) = self.keycode_of(keysym)?;
        if !column_shift {
            return self.tap_keycode(keycode);
        }
        let (shift_keycode, _) = self.keycode_of(XK_SHIFT_L)?;
        self.key_event(shift_keycode, true)?;
        let result = self.tap_keycode(keycode);
        let release = self.key_event(shift_keycode, false);
        result?;
        release
    }

    /// [`Actuator::type_text`] 的剪贴板粘贴路径（可配置时间窗；P2-7 同款
    /// 暴露面）：探测 xclip/wl-copy → 善后骨架 + `ctrl+v` 粘贴。
    pub fn type_text_via_clipboard(
        &self,
        text: &str,
        settle_ms: u64,
        consume_ms: u64,
    ) -> Result<(), Error> {
        let tool = clipboard::detect_clipboard_tool().ok_or(Error::ClipboardUnavailable)?;
        let io = CommandClipboardIo::new(tool);
        clipboard::type_text_via_clipboard(
            &io,
            &mut || self.key_combo("ctrl+v"),
            text,
            settle_ms,
            consume_ms,
        )
    }
}

impl Actuator for XTestActuator {
    fn click(&self, x: f64, y: f64, button: MouseButton, clicks: u32) -> Result<(), Error> {
        // X 协议按钮号：1=左、3=右（2=中键，v1 边界外）。
        let button_code = match button {
            MouseButton::Left => 1u8,
            MouseButton::Right => 3u8,
        };
        self.motion(x, y)?;
        for _ in 0..clicks.max(1) {
            self.button_event(button_code, true)?;
            sleep(Duration::from_millis(CLICK_PAUSE_MS));
            self.button_event(button_code, false)?;
            sleep(Duration::from_millis(CLICK_PAUSE_MS));
        }
        self.sync()
    }

    fn drag(
        &self,
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        via: &[(f64, f64)],
        duration_ms: u64,
        steps: u32,
    ) -> Result<(), Error> {
        // 与 sendinput.rs 同构的采样节奏：航点 = 起点 + via + 终点，每段
        // `steps` 点线性插值，间隔 = 总时长 ÷ 采样总数（via 只提保真不拉长）。
        let mut waypoints = Vec::with_capacity(via.len() + 2);
        waypoints.push((from_x, from_y));
        waypoints.extend(via.iter().copied());
        waypoints.push((to_x, to_y));
        let path = sample_path(&waypoints, steps);
        let pause = step_pause_ms(duration_ms, path.len());
        self.motion(from_x, from_y)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        self.button_event(1, true)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        for (x, y) in &path {
            self.motion(*x, *y)?;
            sleep(Duration::from_millis(pause));
        }
        self.button_event(1, false)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        self.sync()
    }

    fn type_text(&self, text: &str, interval_ms: u64) -> Result<(), Error> {
        // 非 ASCII（中文等）：keysym 只覆盖 Latin-1 → 剪贴板粘贴路径
        //（§7.1；sendinput.rs 因 IME 拦截 UNICODE 注入走同路的对偶决策）。
        if text.chars().any(|c| !c.is_ascii()) {
            return self.type_text_via_clipboard(text, CLIPBOARD_SETTLE_MS, CLIPBOARD_CONSUME_MS);
        }
        for c in text.chars() {
            match classify_char(c) {
                CharKeysym::Skip => continue,
                CharKeysym::Sym(keysym) => self.tap_keysym(keysym)?,
                CharKeysym::Unsupported(c) => {
                    return Err(Error::X11UnsupportedControlChar { code: c as u32 })
                }
            }
            if interval_ms > 0 {
                sleep(Duration::from_millis(interval_ms));
            }
        }
        self.sync()
    }

    fn key_combo(&self, combo: &str) -> Result<(), Error> {
        // 先 parse 后执行（组合键解析错误文案直出，与 sendinput.rs 同语义）。
        let (mods, vk) = parse_combo(combo)?;
        let main = keysym_of_vk(vk).ok_or_else(|| Error::X11Protocol {
            op: "keysym",
            detail: format!("VK 0x{vk:X} 无 keysym 映射（vk_keysym 表缺项）"),
        })?;
        let mut held: Vec<u32> = Vec::new();
        if mods & MOD_CTRL != 0 {
            self.modifier_event(XK_CONTROL_L, true)?;
            held.push(XK_CONTROL_L);
        }
        if mods & MOD_ALT != 0 {
            self.modifier_event(XK_ALT_L, true)?;
            held.push(XK_ALT_L);
        }
        if mods & MOD_SHIFT != 0 {
            self.modifier_event(XK_SHIFT_L, true)?;
            held.push(XK_SHIFT_L);
        }
        if mods & MOD_WIN != 0 {
            self.modifier_event(XK_SUPER_L, true)?;
            held.push(XK_SUPER_L);
        }
        let result = self.tap_keysym(main);
        for keysym in held.iter().rev() {
            if let Err(error) = self.modifier_event(*keysym, false) {
                if result.is_ok() {
                    return Err(error);
                }
            }
        }
        self.sync()?;
        result
    }

    fn set_value_at(&self, x: f64, y: f64, text: &str) -> Result<(), Error> {
        // X11 协议面没有「坐标处控件直写」通道（XTEST 是纯输入流）；Value
        // 直写需要 AT-SPI 控件树（feature linux-atspi）。
        //
        // 【AT-SPI worker 集成点】linux-atspi 启用时委托：
        //   return crate::harness::snapshot::linux::atspi_probe::set_value_at_point(x, y, text);
        // （签名契约 upgrade.md §7：`set_value_at_point(x: f64, y: f64, value:
        // &str) -> crate::harness::Result<()>`——本沙箱该函数尚缺席（并行开发），
        // 落地后把上行注释换成真实调用即可，错误路径保持兜底。）
        let _ = (x, y, text);
        Err(Error::SetValueUnavailable)
    }
}

/* --- 纯逻辑（无 X 依赖，单测锁定） -------------------------------------------- */

/// 拖拽路径采样（sendinput.rs 同语义副本——彼处 cfg(windows) 不可复用）：
/// 相邻航点线性插值，每段 `steps` 个采样点（含段终点不含段起点）；航点
/// 不足 2 个 → 空序列。
fn sample_path(waypoints: &[(f64, f64)], steps: u32) -> Vec<(f64, f64)> {
    let steps = steps.clamp(2, 500) as f64;
    let mut samples = Vec::new();
    for pair in waypoints.windows(2) {
        let (ax, ay) = pair[0];
        let (bx, by) = pair[1];
        for step in 1..=steps as u32 {
            let t = step as f64 / steps;
            samples.push((ax + (bx - ax) * t, ay + (by - ay) * t));
        }
    }
    samples
}

/// 步进间隔（sendinput.rs 同语义副本）= 总时长 ÷ 采样总数，四舍五入后
/// 至少 1ms（防零/负；总时长语义优先于步进节奏）。
fn step_pause_ms(duration_ms: u64, samples: usize) -> u64 {
    if samples == 0 {
        return 1;
    }
    ((duration_ms as f64 / samples as f64).round() as u64).max(1)
}

/// f64 屏幕 coord → i16（四舍五入 + 饱和钳制；X 协议 Motion 字段宽）。
fn clamp_i16(value: f64) -> i16 {
    value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// keysym → (keycode, 需 Shift)（纯函数，单测锁定）：只查基组两列
/// （列 0 = 本键、列 1 = Shift 列）；列 2+ 是 AltGr 组，无 AltGr 模拟不碰
/// （诚实返回 None → 上层报「键不在映射内」而非打错字）。
fn keycode_of_in(keymap: &[Vec<u32>], min_keycode: u8, keysym: u32) -> Option<(u8, bool)> {
    if keysym == 0 {
        return None; // NoSymbol 不参与匹配。
    }
    for column in 0..2 {
        for (index, symbols) in keymap.iter().enumerate() {
            if symbols.get(column).copied() == Some(keysym) {
                // 服务器保证 min+index ≤ max_keycode ≤ 255；饱和加防病态映射。
                return Some((min_keycode.saturating_add(index as u8), column == 1));
            }
        }
    }
    None
}

/// type_text 的 ASCII 字符分类（纯函数，单测锁定）。
enum CharKeysym {
    /// 丢弃（`\r`——`\r\n` 只发一次回车，sendinput.rs 同语义）。
    Skip,
    /// 可直打的 keysym。
    Sym(u32),
    /// 无 keysym 直打路径的控制字符（诚实报错）。
    Unsupported(char),
}

fn classify_char(c: char) -> CharKeysym {
    match c {
        '\r' => CharKeysym::Skip,
        '\n' => CharKeysym::Sym(XK_RETURN),
        '\t' => CharKeysym::Sym(XK_TAB),
        '\u{8}' => CharKeysym::Sym(XK_BACKSPACE),
        '\u{1b}' => CharKeysym::Sym(XK_ESCAPE),
        // 可打印 ASCII：Latin-1 keysym 与码位恒等（含大写——tap_keysym 的
        // Shift 列包裹自动处理）。
        c if (0x20..=0x7E).contains(&(c as u32)) => CharKeysym::Sym(c as u32),
        c => CharKeysym::Unsupported(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* --- 纯逻辑 -------------------------------------------------------------- */

    /// 典型 QWERTY 键盘映射片段（keycode 24=a 行、38=a 键……仅取查表语义
    /// 需要的形态：列 0 小写、列 1 大写）。
    fn toy_keymap() -> (Vec<Vec<u32>>, u8) {
        (
            vec![
                vec![0x61, 0x41],        // a / A（XK_a / XK_A，Shift 列）
                vec![0x73, 0x53],        // s / S
                vec![0xffe3, 0, 0, 0],   // Control_L（基组之外还有列也无妨）
                vec![0x30, 0x29],        // 0 / paren
                vec![0xffe9, 0xffe7],    // Alt_L / Meta_L
            ],
            24, // min_keycode
        )
    }

    #[test]
    fn keycode_lookup_finds_base_and_shift_columns() {
        let (keymap, min) = toy_keymap();
        assert_eq!(keycode_of_in(&keymap, min, 0x61), Some((24, false)), "XK_a 基列");
        assert_eq!(keycode_of_in(&keymap, min, 0x41), Some((24, true)), "XK_A 落 Shift 列");
        assert_eq!(keycode_of_in(&keymap, min, 0xffe3), Some((26, false)), "Control_L");
        assert_eq!(keycode_of_in(&keymap, min, 0xffe9), Some((28, false)), "Alt_L 基列优先");
    }

    #[test]
    fn keycode_lookup_ignores_no_symbol_and_missing() {
        let (keymap, min) = toy_keymap();
        assert_eq!(keycode_of_in(&keymap, min, 0), None, "NoSymbol 不匹配");
        assert_eq!(keycode_of_in(&keymap, min, 0x7A), None, "XK_z 不在映射");
        // AltGr 列（≥2）即使有目标 keysym 也不取（无 AltGr 模拟，打错不如不打）。
        let altgr = vec![vec![0, 0, 0x71, 0x51]]; // 列 2 才有 XK_q
        assert_eq!(keycode_of_in(&altgr, min, 0x71), None);
    }

    #[test]
    fn classify_char_maps_controls_and_printables() {
        assert!(matches!(classify_char('\r'), CharKeysym::Skip));
        assert!(matches!(classify_char('\n'), CharKeysym::Sym(XK_RETURN)));
        assert!(matches!(classify_char('\t'), CharKeysym::Sym(XK_TAB)));
        assert!(matches!(classify_char('\u{8}'), CharKeysym::Sym(XK_BACKSPACE)));
        assert!(matches!(classify_char('\u{1b}'), CharKeysym::Sym(XK_ESCAPE)));
        assert!(matches!(classify_char(' '), CharKeysym::Sym(0x20)));
        assert!(matches!(classify_char('A'), CharKeysym::Sym(0x41)));
        assert!(matches!(classify_char('z'), CharKeysym::Sym(0x7A)));
        assert!(matches!(classify_char('~'), CharKeysym::Sym(0x7E)));
        assert!(matches!(classify_char('\u{0}'), CharKeysym::Unsupported('\u{0}')));
        assert!(matches!(classify_char('\u{7f}'), CharKeysym::Unsupported('\u{7f}')), "DEL 无 keysym");
    }

    #[test]
    fn sample_path_and_step_pause_match_sendinput_semantics() {
        // 直线：每段 steps 点、含终点不含起点。
        let path = sample_path(&[(0.0, 0.0), (100.0, 50.0)], 4);
        assert_eq!(path.len(), 4);
        assert_eq!(path[0], (25.0, 12.5));
        assert_eq!(path[3], (100.0, 50.0));
        // via 踩点 + 总数 = 段数 × steps。
        let waypoints = [(0.0, 0.0), (10.0, 100.0), (30.0, 100.0), (60.0, 0.0)];
        assert_eq!(sample_path(&waypoints, 5).len(), 15);
        // steps 钳制 + 退化输入。
        assert_eq!(sample_path(&[(0.0, 0.0), (10.0, 0.0)], 1).len(), 2);
        assert_eq!(sample_path(&[(0.0, 0.0), (10.0, 0.0)], 9999).len(), 500);
        assert!(sample_path(&[(0.0, 0.0)], 16).is_empty());
        // 步进间隔。
        assert_eq!(step_pause_ms(5000, 16), 313);
        assert_eq!(step_pause_ms(0, 16), 1);
        assert_eq!(step_pause_ms(5, 0), 1);
    }

    #[test]
    fn clamp_i16_rounds_and_saturates() {
        assert_eq!(clamp_i16(3.4), 3);
        assert_eq!(clamp_i16(-3.5), -4, "四舍五入（half away from zero）");
        assert_eq!(clamp_i16(1e9), i16::MAX);
        assert_eq!(clamp_i16(-1e9), i16::MIN);
    }

    /* --- 真实注入（#[ignore]：只在一次性虚拟桌面/专用测试机手动运行） -------- */
    //
    // 运行方式（本机未装 Xvfb，先 `apt install xvfb` 或在测试机执行）：
    //   Xvfb :99 -screen 0 1280x1024x24 &
    //   DISPLAY=:99 CARGO_TARGET_DIR=… cargo test --features linux-x11 -- \
    //       xtest_real --ignored --test-threads 1 --nocapture
    // Xvfb 无真实用户会话，注入不会影响任何桌面；也可以用 xdotool 打开
    // xev/xterm 观察事件流验证。**绝不要对日常桌面（如本机 :1）运行。**

    fn real_display_or_skip() -> Option<String> {
        // 双保险：仅当 DISPLAY 显式指向测试会话（约定 :99/Xvfb）才执行。
        let display = std::env::var("DISPLAY").ok()?;
        if display.trim().is_empty() || !display.contains("99") {
            eprintln!("跳过：DISPLAY（{display}）不是约定的 Xvfb 测试会话 :99");
            return None;
        }
        Some(display)
    }

    #[test]
    #[ignore = "XTest 注入真实键鼠：仅在 Xvfb 一次性虚拟桌面（DISPLAY=:99）或专用测试机运行"]
    fn xtest_real_click_and_motion_on_xvfb() {
        if real_display_or_skip().is_none() {
            return;
        }
        let actuator = XTestActuator::new().expect("Xvfb 会话应可连接且支持 XTEST");
        actuator.click(100.0, 100.0, MouseButton::Left, 1).expect("单击");
        actuator.click(200.0, 150.0, MouseButton::Left, 2).expect("双击");
        actuator.click(50.0, 50.0, MouseButton::Right, 1).expect("右键");
    }

    #[test]
    #[ignore = "XTest 注入真实键鼠：仅在 Xvfb 一次性虚拟桌面（DISPLAY=:99）或专用测试机运行"]
    fn xtest_real_key_combo_and_ascii_typing_on_xvfb() {
        if real_display_or_skip().is_none() {
            return;
        }
        let actuator = XTestActuator::new().expect("构造");
        actuator.key_combo("ctrl+shift+n").expect("三修饰组合键");
        actuator.key_combo("enter").expect("单键");
        actuator.key_combo("alt+f4").expect("F 键组合");
        actuator.type_text("hello XTest 42!", 5).expect("ASCII 逐键");
        actuator.drag(10.0, 10.0, 200.0, 200.0, &[], 300, 10).expect("拖拽");
    }
}
