# channel

![version](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Ffawdlstty%2Fchannel%2Fmain%2F/channel/Cargo.toml&query=package.version&label=version)
![status](https://img.shields.io/github/actions/workflow/status/fawdlstty/channel/rust.yml)

English | [简体中文](README.zh.md)

**One normalized async session API for every AI coding harness — Codex, Claude Code, OpenCode, Zed, and more.**

```rust
use channel::{HarnessKind, Session, SessionConfig, SessionEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = SessionConfig::for_kind(HarnessKind::Codex);
    config.set_observability(false); // invisible to the harness's own UI

    // Creating a session also submits the first message.
    let mut session = Session::create(config, "Introduce this repository").await?;

    while let Some(event) = session.wait_event().await? {
        match event {
            SessionEvent::Message(ev) if ev.reasoning => print!("[reasoning]{}", ev.text),
            SessionEvent::Message(ev) => print!("{}", ev.text),
            SessionEvent::Activity(ev) => println!("{} ({:?})", ev.display, ev.kind),
            SessionEvent::Permission(ev) => println!("permission needed: {}", ev.detail),
            SessionEvent::Finished(finish) => println!("\n-- {} --", finish.text),
        }
    }
    Ok(())
}
```

- **License**: MIT
