#![cfg(feature = "harness")]
//! 实机键鼠冒烟（`#[ignore]`，design.md §19 键鼠回放冒烟行 / 步骤 44）：
//! 拉起真实记事本，经 [`Actuator`] 五原语（SendInput + UIA）完成
//! 「点击聚焦 → 键入中英文 → 组合键全选替换 → ValuePattern 设值读回 →
//! 标题栏拖拽移动窗口」全链路，并以感知原语（`value_at`/窗口矩形）断言。
//!
//! 会移动真实光标并注入键盘输入——仅在交互桌面手动运行：
//! `cargo test -p harness --test actuate_smoke -- --ignored`

#![cfg(windows)]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowRect, SetForegroundWindow, SetWindowPos, HWND_TOPMOST,
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
};
use channel::harness::actuate::{platform_actuator, Actuator, MouseButton};
use channel::harness::snapshot::ControlProbe;

/// 诊断（`--ignored --nocapture`）：对**已运行**的记事本点击/键入后，
/// 打印 UIA 树（JSON）与命中点观测——排查 Win11 新记事本的取值路径。
#[test]
#[ignore = "实机诊断：打印记事本 UIA 树（需交互桌面 + 已运行的记事本）"]
fn debug_notepad_tree() {
    let probe = channel::harness::snapshot::platform_probe().expect("UIA");
    let handle = wait_notepad_window(probe.as_ref(), Duration::from_secs(3)).expect("记事本未运行");
    let rect = window_rect(handle);
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    let edit_x = (rect.left + w / 2) as f64;
    let edit_y = (rect.top + h * 3 / 5) as f64;
    let actuator = platform_actuator().expect("SendInput");
    actuator.click(edit_x, edit_y, MouseButton::Left, 1).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    actuator.type_text("DBG-你好", 5).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let obs = probe.control_at(edit_x, edit_y);
    match obs {
        Some(o) => println!(
            "HIT class={} type={} name={:?} auto={:?} value={:?}",
            o.control.class_name,
            o.control.control_type,
            o.control.name,
            o.control.automation_id,
            probe.value_at(edit_x, edit_y)
        ),
        None => println!("HIT none"),
    }
    let tree = probe.dump_tree(8, 300);
    println!("TREE={}", serde_json::to_string(&tree).unwrap());
}

/// 轮询找出「真实可交互」的记事本窗口：置顶 + 物理命中 + 前台三重验证。
/// 商店版记事本每标签一个顶层窗口，非活动标签被 DWM cloaked——
/// `IsWindowVisible`/矩形照常成立但物理不可见（实机调试：栅格扫到的是
/// 覆盖其上的编辑器窗口）；跨进程 `SetForegroundWindow` 又受前台锁限制，
/// 故以「真实点击中心点」取得前台——点击落在顶窗（=记事本）时顺带完成
/// 前台切换，SendInput 键入才不会打进覆盖窗口（用户的编辑器）。
fn wait_hittable_notepad(
    probe: &dyn ControlProbe,
    actuator: &dyn Actuator,
    deadline: Duration,
) -> Option<i64> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        for w in probe.list_windows() {
            if !w.process_name.eq_ignore_ascii_case("notepad.exe") || w.title.trim().is_empty() {
                continue;
            }
            let hwnd = HWND(w.handle as usize as *mut core::ffi::c_void);
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
            std::thread::sleep(Duration::from_millis(200));
            let rect = window_rect(w.handle);
            let (cx, cy) =
                (((rect.left + rect.right) / 2) as f64, ((rect.top + rect.bottom) / 2) as f64);
            // 物理命中必须回到记事本自身进程（否则该窗 cloaked/被覆盖）。
            let hittable = probe
                .control_at(cx, cy)
                .map(|obs| obs.control.process_name.eq_ignore_ascii_case("notepad.exe"))
                .unwrap_or(false);
            if !hittable {
                continue;
            }
            if unsafe { GetForegroundWindow() } != hwnd {
                let _ = actuator.click(cx, cy, MouseButton::Left, 1);
                std::thread::sleep(Duration::from_millis(250));
            }
            if unsafe { GetForegroundWindow() } == hwnd {
                return Some(w.handle);
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    None
}

/// 轮询等待记事本顶层窗口出现（新窗口启动可达数秒）。
fn wait_notepad_window(probe: &dyn ControlProbe, deadline: Duration) -> Option<i64> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        let hit = probe.list_windows().into_iter().find(|w| {
            w.process_name.eq_ignore_ascii_case("notepad.exe") && !w.title.trim().is_empty()
        });
        if let Some(w) = hit {
            return Some(w.handle);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

fn window_rect(handle: i64) -> RECT {
    let hwnd = HWND(handle as usize as *mut core::ffi::c_void);
    let mut rect = RECT::default();
    // 窗口此刻必然存在（句柄来自 list_windows）；失败即测试环境异常。
    unsafe { GetWindowRect(hwnd, &mut rect) }.expect("GetWindowRect");
    rect
}

/// 轮询读回控件值直到含 `needle`（键入后 UIA 值同步有短滞后，单次读可能
/// 拿到旧值——实机调试结论，见步骤 48）。
fn wait_value_contains(
    probe: &dyn ControlProbe,
    x: f64,
    y: f64,
    needle: &str,
    deadline: Duration,
) -> Option<String> {
    let start = Instant::now();
    loop {
        if let Some(value) = probe.value_at(x, y) {
            if value.contains(needle) {
                return Some(value);
            }
        }
        if start.elapsed() >= deadline {
            return probe.value_at(x, y);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// 网格扫描窗口内的编辑元素（RichEdit/Document/Edit 类）——不盲算坐标：
/// 新实例的编辑区创建有滞后、布局随版本变化（实机调试结论）。
/// 失败前打印窗口内采样命中的控件形态（--nocapture 可见，排查商店版
/// 记事本等新布局）。
fn find_edit_point(probe: &dyn ControlProbe, rect: RECT, deadline: Duration) -> Option<(f64, f64)> {
    let (w, h) = ((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64);
    let fx = [0.30, 0.40, 0.50, 0.60, 0.70];
    let fy = [0.30, 0.40, 0.50, 0.60, 0.70, 0.80];
    let start = Instant::now();
    while start.elapsed() < deadline {
        for &rx in &fx {
            for &ry in &fy {
                let (x, y) = (rect.left as f64 + w * rx, rect.top as f64 + h * ry);
                if let Some(obs) = probe.control_at(x, y) {
                    let editable = obs.control.class_name.contains("RichEdit")
                        || obs.control.class_name.contains("Edit")
                        || obs.control.control_type == "Document"
                        || obs.control.control_type == "Edit";
                    if editable {
                        return Some((x, y));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    // 诊断：超时未命中可编辑元素时，打出网格采样实际命中的控件形态。
    for &rx in &fx {
        for &ry in &fy {
            let (x, y) = (rect.left as f64 + w * rx, rect.top as f64 + h * ry);
            if let Some(obs) = probe.control_at(x, y) {
                println!(
                    "MISSED hit ({x:.0},{y:.0}) type={} class={} name={:?} auto={:?}",
                    obs.control.control_type, obs.control.class_name, obs.control.name, obs.control.automation_id
                );
            }
        }
    }
    None
}

#[test]
#[ignore = "实机键鼠冒烟：真实 SendInput 注入（移动光标/键入），需要交互桌面"]
fn notepad_actuation_end_to_end() {
    let probe = channel::harness::snapshot::platform_probe().expect("UIA 感知应可用");
    let actuator = platform_actuator().expect("SendInput 执行器应可用");

    // 0) 清场：记事本是单实例多标签应用，残留窗口会让键入/断言串台。
    let _ = Command::new("taskkill").args(["/F", "/IM", "notepad.exe"]).output();
    std::thread::sleep(Duration::from_millis(600));

    // 1) 拉起记事本并等窗口出现。优先经 AUMID 直启商店版记事本（explorer
    // shell:appsFolder）：本机（开发机）的 CreateProcess notepad.exe 被
    // IFEO AppExecutionAliasRedirect 重定向到用户的 VS Code 系编辑器——
    // 实机调试：拉起的窗口里网格扫描只见 monaco/xterm，永无编辑元素；
    // shell:appsFolder 路径绕开重定向稳定得到可编辑 Notepad。无商店版
    // 记事本的机器回落 classic notepad.exe。
    let mut child: Option<Child> = None;
    Command::new("explorer")
        .arg("shell:appsFolder\\Microsoft.WindowsNotepad_8wekyb3d8bbwe!App")
        .spawn()
        .ok();
    let mut found = wait_hittable_notepad(probe.as_ref(), actuator.as_ref(), Duration::from_secs(15));
    if found.is_none() {
        let fallback = Command::new("notepad.exe")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 notepad.exe");
        child = Some(fallback);
        found = wait_hittable_notepad(probe.as_ref(), actuator.as_ref(), Duration::from_secs(15));
    }
    let handle = found.expect("15s 内未发现可交互的记事本窗口");
    // 闭包错误类型用 Box<dyn Error>：Actuator 各原语自 P3-13 起返回
    // channel::harness::Error（可 `?` 经 From 收敛），测试本地的 &str 提示同样
    // 经 From 进 Box——一处类型改动覆盖两类错误源。
    let outcome = (|| -> Result<(), Box<dyn std::error::Error>> {
        let hwnd = HWND(handle as usize as *mut core::ffi::c_void);
        // 置顶 + 前台化：新窗口可能整体被宿主终端覆盖（实机调试：网格扫描
        // 会因此扫到覆盖窗口），TOPMOST 保证 hit-test 命中目标。
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd);
        }
        std::thread::sleep(Duration::from_millis(400));
        let rect = window_rect(handle);
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        assert!(w > 200 && h > 200, "记事本窗口尺寸异常: {rect:?}");

        // 2) 定位编辑元素并 click 聚焦（等它就位，避免空点/丢键）。
        let (edit_x, edit_y) = find_edit_point(probe.as_ref(), rect, Duration::from_secs(10))
            .ok_or("10s 内未在窗口内定位到编辑元素")?;
        actuator.click(edit_x, edit_y, MouseButton::Left, 1)?;
        std::thread::sleep(Duration::from_millis(300));

        // 3) type_text：中英文混排 + 标点；轮询读回（UIA 值同步有滞后）。
        actuator.type_text("harness 你好，世界！", 5)?;
        let value = wait_value_contains(probe.as_ref(), edit_x, edit_y, "你好，世界", Duration::from_secs(4));
        let value = value.ok_or("4s 内未读到键入内容")?;
        assert!(value.contains("你好，世界"), "键入应落盘到记事本（读回: {value:?}）");

        // 4) set_value：ValuePattern 直写 + 轮询读回。
        actuator.set_value_at(edit_x, edit_y, "WR-SET-42")?;
        let value = wait_value_contains(probe.as_ref(), edit_x, edit_y, "WR-SET-42", Duration::from_secs(4))
            .ok_or("set_value 后 4s 未读到新值")?;
        assert_eq!(value, "WR-SET-42", "set_value 应直写编辑区");

        // 5) key：ctrl+a 全选 + 键入单字符替换（组合键真实生效的证据）。
        actuator.key_combo("ctrl+a")?;
        std::thread::sleep(Duration::from_millis(150));
        actuator.type_text("D", 5)?;
        let value = wait_value_contains(probe.as_ref(), edit_x, edit_y, "D", Duration::from_secs(4))
            .ok_or("组合键后 4s 未读到新值")?;
        assert_eq!(value, "D", "ctrl+a + 键入应整体替换为 D");

        // 6) drag：底边中点拖拽改窗口高度（角点被 Win11 圆角削弱、顶部是
        // 标签条——实机调试结论；拖前再次前台化防遮挡）。
        let before = window_rect(handle);
        let hwnd2 = HWND(handle as usize as *mut core::ffi::c_void);
        unsafe {
            let _ = SetForegroundWindow(hwnd2);
        }
        std::thread::sleep(Duration::from_millis(250));
        let (sx, sy) = ((before.left + (before.right - before.left) / 2) as f64, (before.bottom - 3) as f64);
        actuator.drag(sx, sy, sx, sy + 80.0, &[], 400, 24)?;
        std::thread::sleep(Duration::from_millis(300));
        let after = window_rect(handle);
        assert!(
            after.bottom - after.top >= (before.bottom - before.top) + 40,
            "底边拖拽应增高窗口: before={before:?} after={after:?}"
        );

        // 6b) drag + via 路径：经中点上拉收窄窗口高度（放大方向在窗口已达
        // 工作区上限的机器上不可复现——实机冒烟改取收缩方向；途经点机制
        // 相同，§10.2「按下 → 按路径移动 → 抬起」的实机证据）。
        let (mx, my) = ((after.left + (after.right - after.left) / 2) as f64, (after.bottom - 3) as f64);
        actuator.drag(mx, my, mx, my - 80.0, &[(mx, my - 40.0)], 400, 24)?;
        std::thread::sleep(Duration::from_millis(300));
        let after_via = window_rect(handle);
        assert!(
            after_via.bottom - after_via.top <= (after.bottom - after.top) - 40,
            "经 via 路径拖拽应收窄窗口: after={after:?} afterVia={after_via:?}"
        );
        Ok(())
    })();

    // 清理：无论成败都收掉记事本（未触发保存，无对话框残留）。AUMID
    // 直启的进程不在 child 名下，靠 taskkill 兜底。
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = Command::new("taskkill").args(["/F", "/IM", "notepad.exe"]).output();

    outcome.expect("实机键鼠冒烟");
}
