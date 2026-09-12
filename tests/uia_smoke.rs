#![cfg(feature = "harness")]
//! 真实 UIA 冒烟（#[ignore]：需真实桌面会话 + 可交互窗口；照
//! app/tests/collectors_smoke.rs 惯例，环境受限时 #[ignore] 降级）。
//!
//! 运行：`cargo test -p harness --test uia_smoke -- --ignored`
//!
//! 验证：真实 UIA 探测对**前台窗口/桌面**的控件解析产出完整 ControlRef
//! （句柄/文字/窗口/进程/路径），并完成 dump_tree 与 list_windows 一次；
//! 另验证 §22.7 密码遮蔽——桌面若存在 IsPassword 控件，感知面（Name/
//! 读值/树转储）只见占位符。本冒烟不注入任何输入（只读感知），
//! 不依赖记事本——有任意前台窗口即过。

use channel::harness::snapshot::platform_probe;
// 密码占位符仅被下方 cfg(windows) 冒烟使用，非 Windows 平台不导入（免 unused 告警）。
#[cfg(windows)]
use channel::harness::snapshot::PASSWORD_PLACEHOLDER;

#[test]
#[ignore = "真实桌面 UIA 冒烟：需交互会话；无头/受限环境跳过"]
fn real_uia_probe_resolves_foreground_and_desktop() {
    let Some(probe) = platform_probe() else {
        panic!("UIA 探测初始化失败（无 UIA 环境？）");
    };
    // 前台窗口 + 清单：无头环境前台可能为空，仅要求不 panic。
    println!("foreground: {:?}", probe.foreground_window());
    let windows = probe.list_windows();
    println!("windows: {} 个", windows.len());
    // P3-10 实机验证：接口文档「按 z 序，先前台」——前台窗口出现在
    // 清单中时（空标题窗口会被过滤）必须居首位，其余保持 z 序。
    if let Some(fg) = probe.foreground_window() {
        if let Some(pos) = windows.iter().position(|w| w.handle == fg.handle) {
            assert_eq!(pos, 0, "前台窗口在 list_windows 清单中时应居首位（P3-10）");
        }
    }
    // 树转储：桌面根至少有若干节点（深度/上限内截断）。
    let tree = probe.dump_tree(3, 64);
    println!("tree nodes: {}", tree.len());
    assert!(!tree.is_empty(), "桌面树不应为空");
    // 焦点控件：无输入焦点时可能 None——允许 None，但 Once 有值时字段完整。
    if let Some(obs) = probe.focused() {
        println!("focused: {:?}", obs.control);
        assert!(!obs.control.control_type.is_empty(), "controlType 必有");
        assert!(obs.control.rect.width >= 0.0 && obs.control.rect.height >= 0.0);
    }
}

/// §22.7 密码遮蔽真实冒烟：直接用 UIA 在桌面树中找 IsPassword 控件，
/// 再经感知层（control_at / value_at / dump_tree）验证只出占位符。
/// 桌面上没有密码控件时打印说明后通过（无法凭空造一个密码框，且本
/// 冒烟禁止注入输入；纯逻辑遮蔽由 snapshot 单测锁定）。
#[cfg(windows)]
#[test]
#[ignore = "真实桌面 UIA 冒烟：需交互会话；无头/受限环境跳过"]
fn real_password_controls_are_masked_everywhere() {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
    };

    let Some(probe) = platform_probe() else {
        panic!("UIA 探测初始化失败（无 UIA 环境？）");
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .expect("CUIAutomation 创建失败");
    let walker = unsafe { automation.ControlViewWalker() }.expect("ControlViewWalker");
    let root = unsafe { automation.GetRootElement() }.expect("桌面根元素");

    // 受限深搜找密码控件：深度 8 / 总访问 4000 节点封顶（防巨型桌面树超时）。
    fn hunt(
        walker: &IUIAutomationTreeWalker,
        element: &IUIAutomationElement,
        depth: u32,
        budget: &mut usize,
        found: &mut Vec<IUIAutomationElement>,
    ) {
        if depth == 0 || *budget == 0 || found.len() >= 4 {
            return;
        }
        *budget -= 1;
        let is_password =
            unsafe { element.CurrentIsPassword() }.map(|b| b.as_bool()).unwrap_or(false);
        if is_password {
            found.push(element.clone());
        }
        let mut child = unsafe { walker.GetFirstChildElement(element) };
        while let Ok(c) = child {
            hunt(walker, &c, depth - 1, budget, found);
            if *budget == 0 || found.len() >= 4 {
                break;
            }
            child = unsafe { walker.GetNextSiblingElement(&c) };
        }
    }

    let mut found: Vec<IUIAutomationElement> = Vec::new();
    hunt(&walker, &root, 8, &mut 4000usize, &mut found);
    if found.is_empty() {
        println!("本机桌面未发现 IsPassword 控件，真实遮蔽校验跳过（纯逻辑遮蔽已有单测）");
        return;
    }
    // 先记原始 Name（遮蔽前的对照；OS 侧也可能已代打码），再验感知面。
    let bstr_to_string = |value: &windows::core::BSTR| {
        String::from_utf16_lossy(value).trim().to_string()
    };
    let raw_names: Vec<String> = found
        .iter()
        .map(|e| unsafe { e.CurrentName() }.map(|n| bstr_to_string(&n)).unwrap_or_default())
        .collect();
    for (i, element) in found.iter().enumerate() {
        let Ok(rect) = (unsafe { element.CurrentBoundingRectangle() }) else {
            continue;
        };
        let cx = (rect.left + rect.right) as f64 / 2.0;
        let cy = (rect.top + rect.bottom) as f64 / 2.0;
        if let Some(obs) = probe.control_at(cx, cy) {
            assert_eq!(obs.control.name, PASSWORD_PLACEHOLDER, "密码控件 Name 必须占位");
        }
        if let Some(value) = probe.value_at(cx, cy) {
            assert_eq!(value, PASSWORD_PLACEHOLDER, "密码控件读值必须占位");
        }
        println!("password control #{i}: raw name = {:?} → 感知面已占位", raw_names[i]);
    }
    // dump_tree：树内不得出现密码控件的真实 Name（控件不在前台窗口树内
    // 时该断言平凡成立，作辅助校验）。
    let raws: Vec<&str> =
        raw_names.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    if !raws.is_empty() {
        let tree = probe.dump_tree(8, 512);
        for raw in raws {
            assert!(
                tree.iter().all(|n| n.name != raw),
                "dump_tree 泄漏密码控件真实 Name: {raw:?}"
            );
        }
        println!("dump_tree {} 节点均未泄漏密码控件原始 Name", tree.len());
    }
}
