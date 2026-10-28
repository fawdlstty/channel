//! Drive a ZCode harness session through channel and verify that both
//! reasoning and answer text stream back in real time.
//!
//! Usage: `zcode_stream [model] [workspace]`
//! Defaults: model `glm团队2/GLM-5.3-Flash`, workspace = current directory.

use std::time::{Duration, Instant};

use channel::{HarnessKind, Session, SessionConfig, SessionEvent};

const DEFAULT_MODEL: &str = "glm团队2/GLM-5.3-Flash";
const PROMPT: &str = "请先在心里思考这道题：17*23等于多少？思考完成后，用不超过三句话解释你的计算过程并给出答案。不要使用任何工具。";
const EVENT_TIMEOUT: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let workspace = args.next();

    let mut config = SessionConfig::for_kind(HarnessKind::ZCode);
    config.set_observability(false);
    config.set_model(Some(model.clone()));
    if let Some(dir) = workspace {
        config.set_workspace(Some(dir.into()));
    }

    println!("[zcode_stream] model={model} workspace={:?}",
        config.get_workspace().map(|p| p.display().to_string()).unwrap_or_else(|| "<cwd>".into()));

    let start = Instant::now();
    let mut session = Session::create(config, PROMPT).await?;
    println!("[zcode_stream] session created in {:?}", start.elapsed());

    let mut reasoning_chunks = 0usize;
    let mut reasoning_chars = 0usize;
    let mut output_chunks = 0usize;
    let mut output_chars = 0usize;
    let mut first_reasoning_at: Option<Duration> = None;
    let mut first_output_at: Option<Duration> = None;
    let mut last_event_at = start.elapsed();
    let mut max_gap = Duration::ZERO;
    let mut in_reasoning = false;

    while let Some(event) = tokio::time::timeout(EVENT_TIMEOUT, session.wait_event()).await?? {
        let at = start.elapsed();
        max_gap = max_gap.max(at - last_event_at);
        last_event_at = at;
        match event {
            SessionEvent::Message(ev) => {
                let ts = at.as_secs_f64();
                if ev.reasoning {
                    if !in_reasoning {
                        println!("\n--- reasoning stream ---");
                        in_reasoning = true;
                    }
                    if first_reasoning_at.is_none() {
                        first_reasoning_at = Some(at);
                        println!("[t+{ts:.3}s] first reasoning delta");
                    }
                    reasoning_chunks += 1;
                    reasoning_chars += ev.text.chars().count();
                    print!("{}", ev.text);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                } else {
                    if in_reasoning {
                        println!("\n--- answer stream ---");
                        in_reasoning = false;
                    }
                    if first_output_at.is_none() {
                        first_output_at = Some(at);
                        println!("[t+{ts:.3}s] first answer delta");
                    }
                    output_chunks += 1;
                    output_chars += ev.text.chars().count();
                    print!("{}", ev.text);
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
            SessionEvent::Activity(ev) => {
                println!("\n[t+{:.3}s] activity: {} ({:?})", at.as_secs_f64(), ev.display, ev.kind);
            }
            SessionEvent::Permission(ev) => {
                println!("\n[t+{:.3}s] permission: {}", at.as_secs_f64(), ev.detail);
            }
            SessionEvent::ContextUsage { used, total } => {
                println!("\n[t+{:.3}s] context usage: {used}/{total}", at.as_secs_f64());
            }
            SessionEvent::CompactionBoundary { .. } => {
                println!("\n[t+{:.3}s] compaction boundary", at.as_secs_f64());
            }
            SessionEvent::Todo { snapshot } => {
                println!("\n[t+{:.3}s] todo snapshot: {} entries", at.as_secs_f64(), snapshot.entries.len());
            }
            SessionEvent::Finished(finish) => {
                println!("\n[t+{:.3}s] finished: {:?} text({} chars)",
                    at.as_secs_f64(), finish.status, finish.text.chars().count());
            }
        }
    }

    println!("\n=== summary ===");
    println!("model: {model}");
    match first_reasoning_at {
        Some(at) => println!("first reasoning delta: t+{:.3}s ({chunks} chunks, {chars} chars)",
            at.as_secs_f64(), chunks = reasoning_chunks, chars = reasoning_chars),
        None => println!("first reasoning delta: NEVER (reasoning not streamed)"),
    }
    match first_output_at {
        Some(at) => println!("first answer delta:   t+{:.3}s ({chunks} chunks, {chars} chars)",
            at.as_secs_f64(), chunks = output_chunks, chars = output_chars),
        None => println!("first answer delta:   NEVER"),
    }
    println!("max inter-event gap: {max_gap:?}, total wall time: {:?}", start.elapsed());
    Ok(())
}
