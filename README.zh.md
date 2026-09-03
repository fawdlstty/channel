# channel

![version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Ffawdlstty%2Fchannel%2Fmain%2F/channel/Cargo.toml&query=package.version&label=version)
![status](https://img.shields.io/github/actions/workflow/status/fawdlstty/channel/rust.yml)

[English](README.md) | 简体中文

**用一套归一化的异步会话 API 驱动所有 AI Coding Harness——Codex、Claude Code、OpenCode、Zed 等。**

```rust
use channel::{HarnessKind, Session, SessionConfig, SessionEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = SessionConfig::for_kind(HarnessKind::Codex);
    config.set_observability(false); // 对 harness 自身界面不可见

    // 创建会话的同时就提交了第一条消息。
    let mut session = Session::create(config, "介绍一下这个仓库").await?;

    while let Some(event) = session.wait_event().await? {
        match event {
            SessionEvent::Message(ev) if ev.reasoning => print!("[推理]{}", ev.text),
            SessionEvent::Message(ev) => print!("{}", ev.text),
            SessionEvent::Activity(ev) => println!("{} ({:?})", ev.display, ev.kind),
            SessionEvent::Permission(ev) => println!("需要授权: {}", ev.detail),
            SessionEvent::Finished(finish) => println!("\n-- {} --", finish.text),
        }
    }
    Ok(())
}
```

- **许可证**：MIT
