//! harness.exe（bin）：独立服务形态的 JSON-lines 感知/操作服务
//! （design.md §22.3 产物候选 → 步骤 35 落地；v1.11 Q 增操作面 + exec 单发）。
//!
//! # 用法
//!
//! ```text
//! harness [--poll-focus-ms N] [--actuation] [--redact 字面量]... [serve] [--help]
//! harness [--actuation] [--redact 字面量]... exec <op> [args-json]
//! ```
//!
//! - 默认为 serve 模式（显式 `serve` 位置子命令等价——Python 运行时
//!   `workflow_runtime.HarnessSession` 以 `[harness.exe, "serve",
//!   "--actuation"]` argv 拉起回放主轨，§10.2）：stdin/stdout 走
//!   [`channel::harness::serve`] 协议（请求-响应 + 订阅事件）；stdin EOF 或
//!   `stop` 行温和退出（宿主死亡不留孤儿——serve 主循环退出时置位进程
//!   级关停标志，焦点轮询线程随之收线，P1-1）；
//! - `exec <op> [args-json]`：执行一次 op，打印一行响应（`{"id":0,"ok":…}`）
//!   后按 ok 退出 0/2——供 Python 运行时经 `subprocess.run` 以一次进程调用
//!   一个原子工具；args-json 非法时明确报错退出 2（不静默按 `{}` 处理）；
//! - `--poll-focus-ms N`：启动焦点轮询线程，客户端 `subscribe focus` 后
//!   每 N ms 推一行 `{"event":"focus","data":{...}}`（0/缺省 = 不轮询；
//!   值非法或缺失以退出码 2 拒绝，不静默吞掉）；
//! - `--actuation`（或环境变量 `HARNESS_ACTUATION=1`）：启用操作类工具面
//!   （click/drag/type_text/key/set_value，§22.7 授权门）——供回放与自愈
//!   现场显式拉起；缺省只读感知；
//! - `--redact <字面量>`（可重复；或环境变量 `HARNESS_REDACT` 逗号分隔
//!   清单，含逗号的字面量请用 `--redact`）：注入出参脱敏钩子（P2-6）——
//!   serve 响应（capture_scene/get_value/list_windows/get_focus/dump_tree
//!   自由文本）与 focus 事件载荷中命中的字面量替换为 `<redacted>` 后才
//!   离开本进程；未给规则 = 恒等（原行为）。exec 模式同样生效（标志可
//!   置于 `exec` 前后）；
//! - 探测不可用（无 UIA / 非 Windows）自动降级：`version.snapshotAvailable`
//!   = false，感知 op 返回 ok:false，协议层照常可用（错误隔离）。
//!
//! # v1.15 边界（用户要求 U）
//!
//! 原生事件直录（R1 ui 事件）、CDP 调试浏览器直录（R2 browser_* 七 op）、
//! BYOK LLM 桥（R5 `relocate_control` + `--llm-*` 参数）已迁至 WorkRecorder
//! 仓的 `wr-harness-ext` crate——本 bin 回归纯感知/操作/识图服务面。

// bin 与示例依赖 cargo 默认发现（无 [[bin]] required-features），feature
// 门控内置于本文件：未启用 `harness` 时编译出仅打印指引的占位 main。
use std::process::ExitCode;
#[cfg(feature = "harness")]
use std::io::BufReader;
#[cfg(feature = "harness")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "harness")]
use std::sync::mpsc;
#[cfg(feature = "harness")]
use std::sync::Arc;
#[cfg(feature = "harness")]
use std::time::Duration;

#[cfg(feature = "harness")]
use serde_json::json;

#[cfg(feature = "harness")]
use channel::harness::actuate::Actuator;
#[cfg(feature = "harness")]
use channel::harness::serve::{self, ServeMessage, ServeState};
#[cfg(feature = "harness")]
use channel::harness::snapshot::{ControlObservation, ControlProbe};
#[cfg(feature = "harness")]
use channel::harness::store::{redact_control_ref, Redactor};
#[cfg(feature = "harness")]
use channel::harness::tools::ToolRegistry;

/// 未启用 `harness` feature 的占位入口：如实指路后退出（无法服务）。
#[cfg(not(feature = "harness"))]
fn main() -> ExitCode {
    eprintln!(
        "harness: the desktop-sensing harness is not compiled in; \
         rebuild with `cargo build --features harness`"
    );
    ExitCode::from(2)
}

/// 声明进程 DPI 感知（P1-2）：必须在任何坐标采集/注入之前调用——否则
/// DPI-unaware 进程被 Windows 按缩放虚拟化，HiDPI 机器（125%/150% 笔记本
/// 出厂常态）上 `SetCursorPos` 点击落点与 UIA/截图的物理坐标错位、
/// `GetSystemMetrics` 截图尺寸失真。三级降级 best-effort：旧系统不支持
/// V2 上下文时逐级回退，全部失败也不影响启动（100% 缩放机器本就无影响）。
#[cfg(all(windows, feature = "harness"))]
fn declare_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
    };
    // SetProcessDPIAware 属 Win32_UI_WindowsAndMessaging（feature 已含）。
    use windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware;
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err()
            && SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE).is_err()
        {
            let _ = SetProcessDPIAware();
        }
    }
}

/// tracing subscriber 初始化（P3-13）：日志一律写 **stderr**——stdout 是
/// JSON-lines 协议通道，绝不能被日志污染（bin 契约红线）。默认级别
/// warn：协议层错误已随 `ok:false` 响应回给调用方，stderr 只留慢性病
/// 现场（轮询停摆/超时线程累积/写失败/probe 初始化失败）；需要更细
/// 现场时以 `RUST_LOG` 覆盖（env-filter 语法，如 `RUST_LOG=harness=debug`）。
/// try_init 失败（全局默认已被占用）静默忽略——日志是旁路可观测性，
/// 不因初始化失败拒绝服务。
#[cfg(feature = "harness")]
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

/// serve 主循环入口（`harness` feature 的真实实现）。
#[cfg(feature = "harness")]
fn main() -> ExitCode {
    // tracing 初始化放最前：后续装配/轮询的结构化日志才有落点。CLI 用法
    // 输出（--help）与参数解析期的用户输入错误提示（此时可能尚未走到
    // 这里）不受影响——那些是面向人的 CLI 输出，不走日志。
    init_tracing();

    #[cfg(windows)]
    declare_dpi_awareness();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut poll_focus_ms: Option<u64> = None;
    let mut actuation = std::env::var("HARNESS_ACTUATION").ok().is_some_and(|v| v.trim() == "1");
    let mut redact_literals: Vec<String> = Vec::new();
    let mut exec: Option<(String, String)> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--poll-focus-ms" => {
                i += 1;
                // 解析失败/缺值明确报错退出（P3-1），不再静默吞掉导致
                // 轮询悄悄不启动。
                let Some(raw) = args.get(i) else {
                    eprintln!("--poll-focus-ms 需要一个正整数毫秒值（如 --poll-focus-ms 250）");
                    return ExitCode::from(2);
                };
                match raw.parse::<u64>() {
                    Ok(ms) => poll_focus_ms = Some(ms),
                    Err(_) => {
                        eprintln!("--poll-focus-ms 值非法: {raw}（需要正整数毫秒）");
                        return ExitCode::from(2);
                    }
                }
            }
            "--redact" => {
                i += 1;
                // 缺值/空值明确报错（对齐 --poll-focus-ms 的 P3-1 风格；
                // 空字面量会让逐字符替换占位符，必须在入口拒绝）。
                let Some(raw) = args.get(i) else {
                    eprintln!("--redact 需要一个字面量参数（如 --redact sk-live-abc，可重复给出）");
                    return ExitCode::from(2);
                };
                if raw.trim().is_empty() {
                    eprintln!("--redact 值不得为空（字面量前后空白按精确匹配，不作 trim）");
                    return ExitCode::from(2);
                }
                redact_literals.push(raw.clone());
            }
            "--actuation" => actuation = true,
            "--help" | "-h" => {
                println!("harness {} — JSON-lines 感知/操作服务", channel::harness::CRATE_VERSION);
                println!("用法: harness [--poll-focus-ms N] [--actuation] [--redact 字面量]...");
                println!("              [serve | exec <op> [args-json]]");
                println!("协议: stdin/stdout JSON lines（详见 --help 后的 serve 模块文档/README）");
                println!("脱敏: --redact <字面量> 可重复（或 HARNESS_REDACT 逗号分隔；含逗号请用 --redact），出参命中文本替换为 <redacted>");
                return ExitCode::SUCCESS;
            }
            "serve" => {
                // 显式 serve 位置子命令：与缺省（无参数）行为完全一致——serve
                // 本就是默认模式；接受该词纯粹为兼容 Python 运行时主轨的
                // `[harness.exe, "serve", "--actuation"]` argv（§10.2）。
            }
            "exec" => {
                let op = args.get(i + 1).cloned();
                // args-json 紧跟 <op>；以 `--` 开头的 token 必是标志而非
                // JSON（合法 JSON 不以 `--` 开头，`-1` 等负数字面量不受
                // 影响）——`exec version --redact x` 形态不再把 "--redact"
                // 误当 args-json（P2-6：exec 模式支持后置 --redact 等标志）。
                let (args_json, consumed) = match args.get(i + 2) {
                    Some(next) if !next.starts_with("--") => (next.clone(), 2),
                    _ => ("{}".to_string(), 1),
                };
                match op {
                    Some(op) => exec = Some((op, args_json)),
                    None => {
                        eprintln!("exec 需要 <op> 参数（--help 查看用法）");
                        return ExitCode::from(2);
                    }
                }
                i += consumed;
            }
            other => {
                eprintln!("未知参数: {other}（--help 查看用法）");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let probe = channel::harness::snapshot::platform_probe();
    let mut registry = match probe.clone() {
        // probe 可用性在 registry 内部逐工具判定；Arc clone 仅共享引用，
        // 轮询线程稍后取回所有权（P3-3：不再 Some(_)+expect 二段式）。
        Some(probe) => ToolRegistry::new(probe),
        None => ToolRegistry::without_probe(),
    };
    if actuation {
        let actuator: Option<Arc<dyn Actuator>> = channel::harness::actuate::platform_actuator();
        if actuator.is_none() {
            eprintln!("harness: 本平台无操作类执行器，--actuation 未生效（保持只读）");
        }
        registry = registry.with_actuation(actuator);
    }

    // --redact 规则集非空 → 注入出参脱敏钩子（P2-6）：serve 响应
    // （capture_scene/get_value）经 registry 生效；焦点事件经轮询线程
    // 持有的同一 redactor 生效（同一规则集，见 [`redact_rule_set`]）。
    // 空 = 不注入（恒等，行为与无该开关完全一致）。
    let redactor: Option<Redactor> =
        redact_rule_set(&redact_literals, std::env::var("HARNESS_REDACT").ok().as_deref())
            .map(|rules| literal_redactor(&rules));
    if let Some(redact) = redactor.clone() {
        registry = registry.with_redactor(redact);
    }

    // 单发模式：执行一次 op → 一行响应 → 按 ok 退出（不进 serve 循环）。
    if let Some((op, args_json)) = exec {
        // args-json 非法明确报错退出 2（P3-2）：静默按 {} 处理会让后续
        // 「缺 x/y」类错误误导调用方。
        let parsed: serde_json::Value = match serde_json::from_str(&args_json) {
            Ok(value) => value,
            Err(e) => {
                eprintln!("exec args-json 非法: {e}（输入: {args_json}）");
                return ExitCode::from(2);
            }
        };
        let outcome = registry.call(&op, &parsed);
        let line = match outcome {
            Ok(result) => json!({ "id": 0, "ok": true, "result": result }),
            // 错误经 Display 写入 wire（P3-13 枚举化后与原字符串形态一致）。
            Err(error) => json!({ "id": 0, "ok": false, "error": error.to_string() }),
        };
        println!("{line}");
        return if line["ok"] == json!(true) { ExitCode::SUCCESS } else { ExitCode::from(2) };
    }

    let state = Arc::new(ServeState::new(registry));
    // 进程级关停标志（P1-1）：serve 主循环任何退出路径（stop 行/stdin EOF/
    // stdout 连续写失败）返回前置位，焦点轮询线程每轮检查后收线——EOF
    // 退出不再依赖「通道 sender 全部释放」的循环闭合。
    let shutdown = Arc::new(AtomicBool::new(false));
    // 有界消息通道（P3-6a）：事件生产者 try_send（满则丢弃，focus 可丢），
    // stdin 请求阻塞 send（背压不丢失）。
    let (tx, rx) = mpsc::sync_channel(serve::CHANNEL_CAPACITY);

    if let (Some(ms), Some(probe)) = (poll_focus_ms.filter(|ms| *ms > 0), probe) {
        spawn_focus_poller(ms, probe, state.clone(), tx.clone(), Arc::clone(&shutdown), redactor);
    }

    serve::run(
        BufReader::new(std::io::stdin()),
        std::io::stdout().lock(),
        state,
        tx,
        rx,
        shutdown,
    );

    ExitCode::SUCCESS
}

/// 焦点轮询事件生产者：订阅 `focus` 期间每 `ms` 推一行事件。
///
/// 收线三保险：serve 主循环退出后关停标志置位（P1-1，本轮检查即退出）；
/// `rx` 丢弃后 send/try_send 失败；两者都不满足时也至多空转到下一轮。
/// `probe.focused()` 是可能无限挂起的 UIA 调用：借 serve 同款「一次性
/// 工作线程 + 限时接收」隔离，超时跳过本轮（P2-1，对齐 ControlProbe
/// 接口契约的外加超时要求）。
#[cfg(feature = "harness")]
fn spawn_focus_poller(
    ms: u64,
    probe: Arc<dyn ControlProbe>,
    state: Arc<ServeState>,
    tx: mpsc::SyncSender<ServeMessage>,
    shutdown: Arc<AtomicBool>,
    redactor: Option<Redactor>,
) {
    /// 单轮 `probe.focused()` 的等待上限（对齐 serve 超时隔离的量级）。
    const PROBE_TIMEOUT_MS: u64 = 2_000;
    /// 连续停摆告警步长：首轮 + 每 N 轮一条 warn（250ms 轮询下约 25s
    /// 一条）——既有可见性，不刷屏。
    const STALL_WARN_STEP: u64 = 100;
    std::thread::spawn(move || {
        let mut dropped_events: u64 = 0;
        let mut consecutive_stalls: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_millis(ms));
            if shutdown.load(Ordering::SeqCst) {
                break; // serve 已退出（stop/EOF/写失败关停）
            }
            if !state.subscribed_kinds().iter().any(|kind| kind == "focus") {
                continue;
            }
            let (obs_tx, obs_rx) = mpsc::channel();
            let probe = Arc::clone(&probe);
            std::thread::spawn(move || {
                let _ = obs_tx.send(probe.focused());
            });
            // 无焦点（Ok(None)）是正常桌面状态，静默跳过；超时（Err）是
            // probe 持续挂起的「轮询停摆」慢性病（P3-13）——首轮与其后
            // 每 STALL_WARN_STEP 轮 warn 一次（结构化字段带轮询周期、
            // 探测超时上限与连续停摆计数），恢复观测即归零。
            let obs = match obs_rx.recv_timeout(Duration::from_millis(PROBE_TIMEOUT_MS)) {
                Ok(Some(obs)) => {
                    consecutive_stalls = 0;
                    obs
                }
                Ok(None) => continue,
                Err(_) => {
                    consecutive_stalls += 1;
                    if consecutive_stalls == 1 || consecutive_stalls % STALL_WARN_STEP == 0 {
                        tracing::warn!(
                            consecutive = consecutive_stalls,
                            poll_ms = ms,
                            probe_timeout_ms = PROBE_TIMEOUT_MS,
                            "焦点轮询探测超时，本轮跳过（probe.focused 疑似持续挂起，轮询停摆）"
                        );
                    }
                    continue;
                }
            };
            let line = focus_event_line(&obs, redactor.as_ref());
            // 有界通道 try_send（P3-6a）：慢客户端导致通道满即丢弃该条事件
            // 并累计计数——阻塞 send 会让慢客户端反向拖死轮询线程。
            match tx.try_send(ServeMessage::Event(line)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    dropped_events += 1;
                    if dropped_events == 1 || dropped_events % 256 == 0 {
                        // P3-13：丢弃现场走结构化日志（原 eprintln 文案语义
                        // 保留，字段带累计数与通道容量）。
                        tracing::warn!(
                            dropped = dropped_events,
                            capacity = serve::CHANNEL_CAPACITY,
                            "事件通道已满，focus 事件累计丢弃（慢客户端）"
                        );
                    }
                }
                Err(mpsc::TrySendError::Disconnected(_)) => break,
            }
        }
    });
}

/* --- --redact 规则装配与焦点事件脱敏（P2-6；纯函数供单测） -------------------- */

/// 合并 `--redact` CLI 字面量与 `HARNESS_REDACT` 环境变量（逗号分隔清单；
/// 条目按空白裁剪，空条目跳过——含逗号的字面量请走 `--redact`，env 侧无法
/// 表达）。合并后为空（两者皆未给/仅空白）→ `None`（不注入钩子，恒等）。
#[cfg(feature = "harness")]
fn redact_rule_set(cli: &[String], env: Option<&str>) -> Option<Vec<String>> {
    let mut rules: Vec<String> = cli.to_vec();
    if let Some(env) = env {
        rules.extend(
            env.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string),
        );
    }
    (!rules.is_empty()).then_some(rules)
}

/// 由字面量规则集构造 [`Redactor`]：出参文本中命中任一规则的字面量替换为
/// `<redacted>`（与感知层 `<password>` 占位风格一致；精确字面量、区分大小
/// 写，逐条依次替换）。空规则集不应传到这里（恒等 = 不注入，见
/// [`redact_rule_set`]）。
#[cfg(feature = "harness")]
fn literal_redactor(rules: &[String]) -> Redactor {
    // 规则集克隆进闭包（钩子要求 'static；规则数与小字面量，成本可忽略）。
    let rules = rules.to_vec();
    Arc::new(move |text: &str| {
        rules
            .iter()
            .fold(text.to_string(), |out, rule| out.replace(rule.as_str(), "<redacted>"))
    })
}

/// focus 事件行构造（轮询线程用；纯函数供单测）：control/label 两个
/// ControlRef 先经 [`redact_control_ref`] 脱敏自由文本（name/windowTitle，
/// 与 capture_scene 出参同一字段集），再序列化——焦点事件不再以明文离开
/// 本进程（P2-6）；`redactor` 为 None 时恒等（缺省原行为）。
#[cfg(feature = "harness")]
fn focus_event_line(obs: &ControlObservation, redactor: Option<&Redactor>) -> String {
    let control = redact_control_ref(&obs.control, redactor);
    let label = obs.label.as_ref().map(|label| redact_control_ref(label, redactor));
    json!({
        "event": "focus",
        "data": { "control": control, "label": label },
    })
    .to_string()
}

#[cfg(all(test, feature = "harness"))]
mod tests {
    use super::*;

    use channel::harness::store::{ControlRect, ControlRef};

    /// 最小 ControlRef 构造（仅自由文本两字段可变，结构字段固定样本值）。
    fn test_ref(name: &str, window_title: &str) -> ControlRef {
        ControlRef {
            handle: 7,
            control_type: "Edit".into(),
            name: name.into(),
            automation_id: "edit.1".into(),
            class_name: "Edit".into(),
            window_title: window_title.into(),
            process_name: "notepad.exe".into(),
            rect: ControlRect { x: 0.0, y: 0.0, width: 100.0, height: 24.0 },
            enabled: Some(true),
            path: None,
        }
    }

    #[test]
    fn redact_rule_set_merges_cli_and_env_and_none_when_empty() {
        // 两侧皆空 → None（不注入，恒等）。
        assert_eq!(redact_rule_set(&[], None), None);
        assert_eq!(redact_rule_set(&[], Some("")), None);
        assert_eq!(redact_rule_set(&[], Some(" , ,")), None, "纯空白条目跳过");
        // CLI 多条保留给出顺序；env 逗号分条并 trim。
        let cli = vec!["sk-live-abc".to_string(), "张三".to_string()];
        assert_eq!(
            redact_rule_set(&cli, None),
            Some(vec!["sk-live-abc".to_string(), "张三".to_string()])
        );
        assert_eq!(redact_rule_set(&[], Some("foo, bar")), Some(vec!["foo".to_string(), "bar".to_string()]));
        assert_eq!(
            redact_rule_set(&cli, Some("foo,,bar")),
            Some(vec!["sk-live-abc".to_string(), "张三".to_string(), "foo".to_string(), "bar".to_string()])
        );
    }

    #[test]
    fn literal_redactor_replaces_each_rule_once_for_all_occurrences() {
        let redact = literal_redactor(&["sk-live-abc".to_string(), "张三".to_string()]);
        assert_eq!(redact("余额 sk-live-abc / 户名 张三"), "余额 <redacted> / 户名 <redacted>");
        assert_eq!(redact("无命中明文"), "无命中明文", "不命中恒等");
        assert_eq!(redact("aSK-LIVE-ABC"), "aSK-LIVE-ABC", "精确字面量（区分大小写）");
        // 多次出现全部替换；多规则叠加先后无关紧要（规则互不包含时）。
        assert_eq!(redact("k1 sk-live-abc k2 sk-live-abc"), "k1 <redacted> k2 <redacted>");
    }

    #[test]
    fn focus_event_line_redacts_free_text_fields_only() {
        let obs = ControlObservation {
            control: test_ref("备注 sk-live-abc", "报销单 张三"),
            label: Some(test_ref("备注", "报销单 张三")),
        };
        let redact = literal_redactor(&["sk-live-abc".to_string(), "张三".to_string()]);
        let line = focus_event_line(&obs, Some(&redact));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["event"], json!("focus"));
        // 自由文本（name/windowTitle，与 capture_scene 出参同字段集）脱敏。
        assert_eq!(value["data"]["control"]["name"], json!("备注 <redacted>"));
        assert_eq!(value["data"]["control"]["windowTitle"], json!("报销单 <redacted>"));
        assert_eq!(value["data"]["label"]["name"], json!("备注"));
        assert_eq!(value["data"]["label"]["windowTitle"], json!("报销单 <redacted>"), "label 同字段集");
        // 结构字段不脱敏（脱敏无信息增益且破坏锚点，见 store::redact_control_ref）。
        assert_eq!(value["data"]["control"]["controlType"], json!("Edit"));
        assert_eq!(value["data"]["control"]["handle"], json!(7));
        assert_eq!(value["data"]["control"]["processName"], json!("notepad.exe"));
        // 未注入（缺省）= 恒等：明文原样（契约警告的对照）。
        let plain = focus_event_line(&obs, None);
        let value: serde_json::Value = serde_json::from_str(&plain).unwrap();
        assert_eq!(value["data"]["control"]["name"], json!("备注 sk-live-abc"));
        // label 缺席时序列化为 null（与原 json! 形态一致）。
        let no_label = ControlObservation { control: obs.control.clone(), label: None };
        let line = focus_event_line(&no_label, Some(&redact));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["data"]["label"], serde_json::json!(null));
    }
}
