//! SendInput 键鼠注入（操作类五原语的 Windows 生产实现，§22.4 H3 v1.11）。
//!
//! - 定位一律 `SetCursorPos`（物理像素，免归一化换算）；按下/抬起经
//!   `SendInput` 注入——与录制侧 input-hook 的 `injected` 标记同源（LL
//!   钩子可识别注入事件，回放不会在再次录制时形成事件回环，§6.3）；
//! - 键入用 `KEYEVENTF_UNICODE` 逐 UTF-16 单元注入（中文/符号可用）；
//!   `\r\n` 只对 `\n` 发 VK_RETURN，`\r` 跳过；
//! - 组合键 = 修饰键按下 → 主键 tap → 修饰键逆序抬起；
//! - `set_value` 走 UIA ValuePattern（[`crate::harness::snapshot::uia`] 的
//!   `set_value_at_point`）；失败文案含原因（不支持 ValuePattern 等）。

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEINPUT, VIRTUAL_KEY, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

use crate::harness::actuate::{
    parse_combo, Actuator, MouseButton, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN, VK_CONTROL,
    VK_LWIN, VK_MENU, VK_RETURN, VK_SHIFT,
};
use crate::harness::Error;

/// 单次点击按下/抬起之间的停顿（目标应用识别为完整点击的最小时序）。
const CLICK_PAUSE_MS: u64 = 15;
/// 组合键各步之间的停顿。
const KEY_PAUSE_MS: u64 = 10;
/// 剪贴板置入后、粘贴前的等待（经验值：剪贴板跨进程就绪 + 前台应用
/// 收到更新的最小时序）。
const CLIPBOARD_SETTLE_MS: u64 = 120;
/// 粘贴后、恢复用户剪贴板前的等待（经验值：留足目标应用消费粘贴的
/// 时间窗，避免恢复动作抢跑把原文本粘贴进目标）。
const CLIPBOARD_CONSUME_MS: u64 = 250;

/// 生产执行器（无状态；逐调用注入，线程安全）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SendInputActuator;

impl Actuator for SendInputActuator {
    fn click(&self, x: f64, y: f64, button: MouseButton, clicks: u32) -> Result<(), Error> {
        cursor_to(x, y)?;
        let (down, up) = match button {
            MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        };
        for _ in 0..clicks.max(1) {
            inject_mouse(down)?;
            sleep(Duration::from_millis(CLICK_PAUSE_MS));
            inject_mouse(up)?;
            sleep(Duration::from_millis(CLICK_PAUSE_MS));
        }
        Ok(())
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
        // 航点 = 起点 + via 途经点 + 终点；每段均匀采样 `steps` 点，间隔 =
        // 总时长 ÷ 全部采样点（duration_ms 保持「整次拖拽总时长」语义，
        // §10.2；经 via 加点只提高轨迹保真，不拉长时间）。
        let mut waypoints = Vec::with_capacity(via.len() + 2);
        waypoints.push((from_x, from_y));
        waypoints.extend(via.iter().copied());
        waypoints.push((to_x, to_y));
        let path = sample_path(&waypoints, steps);
        let pause = step_pause_ms(duration_ms, path.len());
        cursor_to(from_x, from_y)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        inject_mouse(MOUSEEVENTF_LEFTDOWN)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        for (x, y) in path {
            cursor_to(x, y)?;
            sleep(Duration::from_millis(pause));
        }
        inject_mouse(MOUSEEVENTF_LEFTUP)?;
        sleep(Duration::from_millis(CLICK_PAUSE_MS));
        Ok(())
    }

    fn type_text(&self, text: &str, interval_ms: u64) -> Result<(), Error> {
        // 含非 ASCII（中文等）→ 剪贴板粘贴路径：UNICODE 直注入会被系统
        // 输入法拦截改写（实机调试：你好→！！），粘贴路径确定性成立
        // （design §10.2「中文经剪贴板粘贴回退」的实现侧同语义）。
        if text.chars().any(|c| c as u32 > 0x7F) {
            return self.type_text_via_clipboard(
                text,
                CLIPBOARD_SETTLE_MS,
                CLIPBOARD_CONSUME_MS,
            );
        }
        for unit in text.encode_utf16() {
            match unit {
                // \r\n → 只对 \n 发回车；\r 跳过（避免双回车）。
                0x0D => continue,
                0x0A => tap_key(VK_RETURN)?,
                other => inject_unicode(other)?,
            }
            if interval_ms > 0 {
                sleep(Duration::from_millis(interval_ms));
            }
        }
        Ok(())
    }

    fn key_combo(&self, combo: &str) -> Result<(), Error> {
        let (mods, main) = parse_combo(combo)?;
        let mut held: Vec<u32> = Vec::new();
        if mods & MOD_CTRL != 0 {
            key_down(VK_CONTROL)?;
            held.push(VK_CONTROL);
        }
        if mods & MOD_ALT != 0 {
            key_down(VK_MENU)?;
            held.push(VK_MENU);
        }
        if mods & MOD_SHIFT != 0 {
            key_down(VK_SHIFT)?;
            held.push(VK_SHIFT);
        }
        if mods & MOD_WIN != 0 {
            key_down(VK_LWIN)?;
            held.push(VK_LWIN);
        }
        let result = tap_key(main);
        for vk in held.into_iter().rev() {
            if let Err(error) = key_up(vk) {
                if result.is_ok() {
                    return Err(error);
                }
            }
        }
        result
    }

    fn set_value_at(&self, x: f64, y: f64, text: &str) -> Result<(), Error> {
        crate::harness::snapshot::uia::set_value_at_point(x, y, text)
    }
}

impl SendInputActuator {
    /// [`Actuator::type_text`] 剪贴板粘贴路径的可配置形态（P2-7）：
    /// `settle_ms` = 置入剪贴板后、粘贴前的等待，`consume_ms` = 粘贴后、
    /// 恢复用户剪贴板前的等待；trait 方法以默认常量
    /// （[`CLIPBOARD_SETTLE_MS`]/[`CLIPBOARD_CONSUME_MS`]）调用本方法，
    /// 需要适配慢应用/慢远程桌面的调用方可直接调本方法自定义时间窗。
    ///
    /// 隐私与取舍（协议级已知，调用方须知）：
    /// - **键入文本会短暂进入系统剪贴板**，并可能被 Win+V 剪贴板历史
    ///   （Cloud Clipboard）留存——经 `type_text` 键入非 ASCII 内容存在
    ///   隐私面；敏感场景宜改用 `set_value`（UIA ValuePattern 直写，
    ///   不经剪贴板）；
    /// - 恢复仅还原 **CF_UNICODETEXT 文本**：用户剪贴板中的其他格式
    ///   （文件/图片等）在 EmptyClipboard 后无法复原；
    /// - 任何置入/粘贴失败都先 best-effort 恢复 `previous` 再返回原错误
    ///   （善后失败不掩盖原错误，也不丢恢复动作）。
    pub fn type_text_via_clipboard(
        &self,
        text: &str,
        settle_ms: u64,
        consume_ms: u64,
    ) -> Result<(), Error> {
        // 先留副本：EmptyClipboard 会覆盖用户剪贴板，粘贴后尽力恢复
        // （副本读取失败仍继续粘贴——功能优先，宁可丢剪贴板也不丢键入）。
        let previous = get_clipboard_text();
        if let Err(error) = set_clipboard_text(text) {
            // 置入失败时剪贴板可能已被 EmptyClipboard 清空（P2-7：不再
            // 提前 `?` 丢弃恢复动作）→ 先恢复副本再返回原错误。
            if let Some(previous) = previous {
                let _ = set_clipboard_text(&previous);
            }
            return Err(error);
        }
        sleep(Duration::from_millis(settle_ms));
        let result = self.key_combo("ctrl+v");
        // 等目标应用消费完粘贴再恢复（留足余量避免把恢复的原文本
        // 粘贴进目标——粘贴慢于该窗口的应用需经可配置入口加大）。
        sleep(Duration::from_millis(consume_ms));
        if let Some(previous) = previous {
            // 恢复为 best-effort：失败仅忽略（不能因善后失败让键入
            // 本身报错）；且只还原 CF_UNICODETEXT 文本，原始剪贴板里
            // 的其他格式（文件/图片等）无法复原——协议级已知取舍。
            let _ = set_clipboard_text(&previous);
        }
        result
    }
}

/// 拖拽路径采样（纯函数，单测锁定）：相邻航点间线性插值，每段产出
/// `steps` 个采样点（段内含段终点、不含段起点——与既有 `1..=steps`
/// 采样语义一致）；采样序列依次经过每个航点（via 途经点被真实踩到）。
/// 航点不足 2 个 → 空序列（调用方按直落处理）。
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

/// 步进间隔（纯函数，单测锁定）= 总时长 ÷ 采样总数，四舍五入后至少 1ms
/// （防零/负；不再设 60ms 上限——「duration_ms 为总时长」的语义优先于步
/// 进节奏）。超长间隔经 `Sleep` 等待：Windows 默认计时精度约 15.6ms，
/// 长间隔下的误差相对占比可忽略；需要更细节奏时由调用方加大 `steps`。
fn step_pause_ms(duration_ms: u64, samples: usize) -> u64 {
    if samples == 0 {
        return 1;
    }
    let pause = (duration_ms as f64 / samples as f64).round() as u64;
    pause.max(1)
}

fn cursor_to(x: f64, y: f64) -> Result<(), Error> {
    let (x, y) = (x.round() as i32, y.round() as i32);
    // SetCursorPos 对负坐标/超屏坐标会裁剪到虚拟屏，视为成功（录制侧同语义）。
    unsafe { SetCursorPos(x, y) }
        .map_err(|source| Error::SetCursorPosFailed { x, y, source })
}

fn inject_mouse(flags: MOUSE_EVENT_FLAGS) -> Result<(), Error> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[input])
}

fn inject_unicode(unit: u16) -> Result<(), Error> {
    let make = |flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[make(KEYEVENTF_UNICODE)])?;
    send(&[make(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)])
}

fn key_down(vk: u32) -> Result<(), Error> {
    send_vk(vk, false)
}

fn key_up(vk: u32) -> Result<(), Error> {
    send_vk(vk, true)
}

fn tap_key(vk: u32) -> Result<(), Error> {
    key_down(vk)?;
    sleep(Duration::from_millis(KEY_PAUSE_MS));
    key_up(vk)?;
    sleep(Duration::from_millis(KEY_PAUSE_MS));
    Ok(())
}

fn send_vk(vk: u32, up: bool) -> Result<(), Error> {
    let make = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send(&[make(up)])
}

fn send(inputs: &[INPUT]) -> Result<(), Error> {
    if inputs.is_empty() {
        return Ok(());
    }
    let injected = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if injected as usize != inputs.len() {
        return Err(Error::SendInputIncomplete {
            injected: injected as usize,
            expected: inputs.len(),
        });
    }
    Ok(())
}

/// 写 CF_UNICODETEXT 剪贴板（粘贴回退路径；CF_TEXT = 13）。
fn set_clipboard_text(text: &str) -> Result<(), Error> {
    unsafe {
        // 关联当前线程任务；打开失败多为其他进程持有剪贴板——重试意义有限，直报。
        OpenClipboard(Some(HWND::default()))
            .map_err(Error::OpenClipboardFailed)?;
        let result = (|| -> Result<(), Error> {
            EmptyClipboard().map_err(Error::EmptyClipboardFailed)?;
            let bytes = size_of::<u16>() * (text.encode_utf16().count() + 1);
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(Error::GlobalAllocFailed)?;
            let dst = GlobalLock(handle);
            if dst.is_null() {
                // 锁失败 → 剪贴板未接管内存 → 释放后报错（防泄漏）。
                let _ = GlobalFree(Some(handle));
                return Err(Error::ClipboardLockFailed);
            }
            let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, dst.cast(), bytes);
            let _ = GlobalUnlock(handle);
            // SetClipboardData 失败 → 所有权仍在调用方 → 释放句柄再报错
            // （防泄漏）；成功后剪贴板接管内存（不再 GlobalFree）。
            if let Err(source) =
                SetClipboardData(13 /* CF_UNICODETEXT */, Some(HANDLE(handle.0)))
            {
                let _ = GlobalFree(Some(handle));
                return Err(Error::SetClipboardDataFailed(source));
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// 读当前剪贴板 CF_UNICODETEXT 文本（type_text 粘贴前留副本用）。
/// 无文本 / 打开失败 / 锁失败一律 `None`——读副本是善后优化而非功能本体，
/// 调用方对 `None` 跳过恢复即可（错误细节对键入主流程无意义）。
fn get_clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(Some(HWND::default())).ok()?;
        let result = (|| -> Option<String> {
            let handle = HGLOBAL(GetClipboardData(13 /* CF_UNICODETEXT */).ok()?.0);
            let src = GlobalLock(handle) as *const u16;
            if src.is_null() {
                return None;
            }
            // 以首个 \0 截断（CF_UNICODETEXT 以空终止）；长度上界取实际
            // 分配块大小，防病态无终止数据越界读。
            let max_units = GlobalSize(handle) / size_of::<u16>();
            let mut len = 0usize;
            while len < max_units && *src.add(len) != 0 {
                len += 1;
            }
            let _ = GlobalUnlock(handle);
            Some(String::from_utf16_lossy(std::slice::from_raw_parts(src, len)))
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_path_without_via_is_straight_line() {
        // 无 via = 直线：每段 steps 个采样点，含终点不含起点（既有语义）。
        let path = sample_path(&[(0.0, 0.0), (100.0, 50.0)], 4);
        assert_eq!(path.len(), 4);
        assert_eq!(path[0], (25.0, 12.5));
        assert_eq!(path[3], (100.0, 50.0), "采样应踩到终点");
    }

    #[test]
    fn sample_path_passes_through_via_points_in_order() {
        // via 航点被真实踩到，且顺序保持；总采样数 = 段数 × steps。
        let waypoints = [(0.0, 0.0), (10.0, 100.0), (30.0, 100.0), (60.0, 0.0)];
        let path = sample_path(&waypoints, 5);
        assert_eq!(path.len(), 3 * 5);
        assert_eq!(path[4], (10.0, 100.0), "第 1 段终点 = via[0]");
        assert_eq!(path[9], (30.0, 100.0), "第 2 段终点 = via[1]");
        assert_eq!(path[14], (60.0, 0.0), "末段终点 = 终点");
        // 中点插值抽样：第 1 段 t=0.4 → (4, 40)。
        assert_eq!(path[1], (4.0, 40.0));
    }

    #[test]
    fn sample_path_clamps_steps_and_handles_degenerate_input() {
        // steps 钳制 2..=500（既有语义）；航点不足 2 个 → 空序列。
        assert_eq!(sample_path(&[(0.0, 0.0), (10.0, 0.0)], 1).len(), 2);
        assert_eq!(sample_path(&[(0.0, 0.0), (10.0, 0.0)], 9999).len(), 500);
        assert!(sample_path(&[(0.0, 0.0)], 16).is_empty());
        assert!(sample_path(&[], 16).is_empty());
    }

    #[test]
    fn step_pause_is_duration_over_samples_without_upper_clamp() {
        // 总时长语义：5000ms / 16 步 = 313ms（不再被 60ms 上限压到 ~1s）。
        assert_eq!(step_pause_ms(5000, 16), 313);
        assert_eq!(step_pause_ms(400, 24), 17);
        assert_eq!(step_pause_ms(0, 16), 1, "防零下限");
        assert_eq!(step_pause_ms(5, 0), 1, "空采样防除零");
        assert_eq!(step_pause_ms(5000, 2), 2500, "超长间隔不再钳 60ms");
    }
}
