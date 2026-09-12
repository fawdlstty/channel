#![cfg(feature = "harness")]
//! bin 协议冒烟：拉起真实 `harness.exe` 子进程，走一轮
//! 「请求 → 响应 → stop 干净退出」回环（协议生命周期/错误隔离的进程级
//! 验证；H4 serve 的单测见 `src/serve` 内存回环）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn bin_responds_to_version_and_exits_on_stop() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harness"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 harness.exe");

    // stdout → 行通道（后台线程逐行转发，主测试侧带超时消费）。
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // 请求 version + 操作类探测 + stop（写完即关 stdin → EOF 双保险退出路径）。
    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"id":7,"op":"version"}}"#).expect("写请求");
        writeln!(stdin, r#"{{"id":8,"op":"click","args":{{}}}}"#).expect("写请求");
        writeln!(stdin, "stop").expect("写 stop");
        stdin.flush().ok();
    }

    // 第一行：version 响应（探测可用与否不影响该 op）。
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("应收到一行响应");
    let value: serde_json::Value = serde_json::from_str(&line).expect("响应必须是 JSON");
    assert_eq!(value["id"], serde_json::json!(7));
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["result"]["name"], serde_json::json!("harness"));
    assert_eq!(
        value["result"]["protocol"],
        serde_json::json!(channel::harness::PROTOCOL_VERSION)
    );

    // 第二行：操作类工具未授权（进程未带 --actuation，§22.7 授权门）
    // → ok:false，错误文案指明授权开关；不崩协议（错误隔离）。
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("应收到错误响应");
    let value: serde_json::Value = serde_json::from_str(&line).expect("响应必须是 JSON");
    assert_eq!(value["id"], serde_json::json!(8));
    assert_eq!(value["ok"], serde_json::json!(false));
    assert!(
        value["error"].as_str().unwrap().contains("--actuation"),
        "授权门错误文案应指明开关: {}",
        value["error"]
    );

    // stop → 进程 0 退出（温和退出，无孤儿）。
    let status = child.wait().expect("等待退出");
    assert!(status.success(), "stop 后应干净退出: {status:?}");
}

/// `--actuation` 授权进程：version 报告 actuation=true，操作类进入注册表
/// （参数校验先行——非法组合键在授权进程内返回参数错误而非授权错误）。
/// 操作执行器仅 Windows 提供，非 Windows 平台授权恒只读，整测随平台门控。
#[cfg(windows)]
#[test]
fn bin_actuation_flag_unlocks_operational_surface() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--actuation")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 harness.exe --actuation");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"id":1,"op":"version"}}"#).expect("写请求");
        writeln!(stdin, r#"{{"id":2,"op":"key","args":{{"combo":"ctrl+shift"}}}}"#).expect("写请求");
        writeln!(stdin, "stop").expect("写 stop");
        stdin.flush().ok();
    }

    let line = rx.recv_timeout(Duration::from_secs(30)).expect("version 响应");
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["result"]["actuation"], serde_json::json!(true));
    assert_eq!(
        value["result"]["tools"].as_array().map(|a| a.len()),
        Some(12),
        "授权后感知 7 + 操作 5（v1.13 新增 capture_screen）"
    );

    // 参数校验在注册表层（主键为修饰键 → 参数错误；已授权进程非授权错误）。
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("key 响应");
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["ok"], serde_json::json!(false));
    assert!(value["error"].as_str().unwrap().contains("修饰键"));

    let status = child.wait().expect("等待退出");
    assert!(status.success(), "stop 后应干净退出: {status:?}");
}

/// exec 单发模式（bin/harness.rs 步骤 45）：一次进程调用 version op，
/// stdout 打印一行 `{"id":0,"ok":true,...}` 并按 ok 退出 0——Python
/// 运行时 `subprocess.run` 主轨形态。
#[test]
fn bin_exec_version_prints_one_ok_line_and_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["exec", "version"])
        .stderr(Stdio::null())
        .output()
        .expect("拉起 harness exec version");
    assert!(output.status.success(), "exec version 应退出 0: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "exec 单发只打印一行响应: {stdout}");
    let value: serde_json::Value = serde_json::from_str(lines[0]).expect("响应必须是 JSON");
    assert_eq!(value["id"], serde_json::json!(0));
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["result"]["name"], serde_json::json!("harness"));
}

/// exec 的 args-json 非法（P3-2）：stderr 明确报 args 非法并以退出码 2
/// 结束，不再静默按 `{}` 处理（那会让后续「缺 x/y」类错误误导调用方）。
/// 用 version op 隔离非法 args 路径——click 在非 Windows 未授权，错误会
/// 先撞授权门。
#[test]
fn bin_exec_rejects_invalid_args_json_with_exit_2() {
    let output = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["exec", "version", "{\"bad json"])
        .output()
        .expect("拉起 harness exec version 非法 args");
    assert_eq!(output.status.code(), Some(2), "非法 args-json 应退出 2");
    assert!(
        output.stdout.is_empty(),
        "args 非法不应打印响应行: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("args-json") && stderr.contains("非法"),
        "stderr 应指明 args-json 非法: {stderr}"
    );
}

/// `--poll-focus-ms abc` / 缺值（P3-1）：解析失败以退出码 2 结束，
/// 不再被 `.parse().ok()` 静默吞掉导致轮询悄悄不启动。
#[test]
fn bin_rejects_invalid_poll_focus_ms_with_exit_2() {
    for flag_args in [&["--poll-focus-ms", "abc"][..], &["--poll-focus-ms"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(flag_args)
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .output()
            .expect("拉起 harness 解析参数");
        assert_eq!(
            output.status.code(),
            Some(2),
            "参数 {flag_args:?} 应以退出码 2 拒绝"
        );
    }
}

/// Python 运行时回放主轨的真实 argv（workflow_runtime.HarnessSession._spawn，
/// `[harness.exe, "serve", "--actuation"]`）：显式 `serve` 位置子命令必须
/// 被接受且与缺省行为一致——此前未知参数直接 exit 2，主轨实机必挂。
#[test]
fn bin_serve_positional_subcommand_accepts_runtime_argv() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["serve", "--actuation"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 harness.exe serve --actuation");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"id":1,"op":"ping"}}"#).expect("写请求");
        writeln!(stdin, r#"{{"id":2,"op":"version"}}"#).expect("写请求");
        writeln!(stdin, "stop").expect("写 stop");
        stdin.flush().ok();
    }

    // ping：serve 模式正常应答（进程未因 `serve` 未知参数退出）。
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("ping 响应");
    let value: serde_json::Value = serde_json::from_str(&line).expect("响应必须是 JSON");
    assert_eq!(value["id"], serde_json::json!(1));
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["result"]["pong"], serde_json::json!(true));

    // version：serve 子命令叠加 --actuation 后操作面已授权（回放主轨前提）。
    // actuation 解锁依赖操作执行器，仅 Windows 可满足；非 Windows 保持只读。
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("version 响应");
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["ok"], serde_json::json!(true));
    #[cfg(windows)]
    assert_eq!(value["result"]["actuation"], serde_json::json!(true));

    let status = child.wait().expect("等待退出");
    assert!(status.success(), "stop 后应干净退出: {status:?}");
}
