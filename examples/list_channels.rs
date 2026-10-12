//! Prints `detect_harness` and `list_channels` results for every harness
//! kind against the real machine state.
//!
//! Run: `cargo run --features ccswitch --example list_channels`
//!
//! Without the `ccswitch` feature the run still works, but Codex channel
//! enumeration degrades to the fallback `default` entry (TOML parsing and
//! the cc-switch key set are `ccswitch`-gated).

use channel::{detect_harness, list_channels, HarnessKind};

const KINDS: [HarnessKind; 7] = [
    HarnessKind::ZCode,
    HarnessKind::Codex,
    HarnessKind::ClaudeCode,
    HarnessKind::OpenCode,
    HarnessKind::ZedAcp,
    HarnessKind::DeepSeek,
    HarnessKind::Hermes,
];

fn display(kind: &HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Codex => "Codex",
        HarnessKind::ClaudeCode => "ClaudeCode",
        HarnessKind::OpenCode => "OpenCode",
        HarnessKind::ZedAcp => "ZedAcp",
        HarnessKind::ZCode => "ZCode",
        HarnessKind::DeepSeek => "DeepSeek",
        HarnessKind::Hermes => "Hermes",
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    for kind in KINDS {
        let name = display(&kind);
        println!("== {name} ==");

        match detect_harness(kind.clone()).await {
            Ok(Some(info)) => {
                let version = info
                    .version
                    .unwrap_or_else(|| "unknown version".to_owned());
                println!("  installed: {} ({version})", info.bin_path.display());
            }
            Ok(None) => println!("  not installed"),
            Err(error) => println!("  detection failed: {error}"),
        }

        match list_channels(kind.clone()).await {
            Ok(channels) if channels.is_empty() => println!("  channels: (none)"),
            Ok(channels) => {
                println!("  channels: {}", channels.len());
                for channel in channels {
                    let switch = channel
                        .switch_key
                        .as_deref()
                        .map(|key| format!(" [cc-switch: {key}]"))
                        .unwrap_or_default();
                    println!(
                        "  {} / {} / {}{switch}",
                        name, channel.team, channel.model
                    );
                }
            }
            Err(error) => println!("  channel enumeration failed: {error}"),
        }
        println!();
    }
    Ok(())
}
