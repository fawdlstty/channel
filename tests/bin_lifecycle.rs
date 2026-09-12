#![cfg(feature = "harness")]
//! bin 生命周期回归（P1-1）：带焦点轮询启动的 serve 进程必须能经
//! stdin EOF 温和退出（退出码 0）——关停标志修复前，轮询线程手里的
//! 通道 sender 使「所有 sender 释放 → 通道关闭」永不满足，EOF 后主循环
//! 永远阻塞在 `recv()` 上成孤儿进程。
//!
//! 非 Windows 上感知降级（platform_probe 返回 None → 轮询线程不启动），
//! 协议层照常：EOF 退出路径跨平台锁定；轮询线程持有 sender 的触发条件
//! （探测可用）由 Windows CI 覆盖。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// `serve --poll-focus-ms 50` + 一轮 version 请求-响应 → drop stdin（EOF）
/// → 进程 5s 内以退出码 0 退出。
#[test]
fn serve_with_focus_poller_exits_on_stdin_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["serve", "--poll-focus-ms", "50"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起 harness.exe serve --poll-focus-ms 50");

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

    // 一轮请求-响应证明协议层活着，随后块结束 drop stdin → EOF。
    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, r#"{{"id":1,"op":"version"}}"#).expect("写请求");
        stdin.flush().ok();
    }

    let line = rx.recv_timeout(Duration::from_secs(30)).expect("应收到 version 响应");
    let value: serde_json::Value = serde_json::from_str(&line).expect("响应必须是 JSON");
    assert_eq!(value["id"], serde_json::json!(1));
    assert_eq!(value["ok"], serde_json::json!(true));

    // EOF → 温和退出（修复前：轮询 sender 未释放 → recv() 永阻 → 孤儿）。
    let Some(status) = wait_with_timeout(&mut child, Duration::from_secs(5)) else {
        panic!("stdin EOF 后 5s 内未退出（孤儿进程回归，P1-1）");
    };
    assert!(status.success(), "EOF 后应干净退出（退出码 0）: {status:?}");
}

/// 带超时的 wait：超时杀进程返回 None（防止修复失效时测试整体挂起）。
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}
