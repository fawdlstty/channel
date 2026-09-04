#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = channel::SessionConfig::for_kind(channel::HarnessKind::Codex);
    config.set_observability(false);
    let mut session = channel::Session::create(config, "你好").await?;
    let mut is_reasoning = false;
    let mut is_newline = true;
    while let Some(ev) = session.wait_event().await? {
        match ev {
            channel::SessionEvent::Activity(ev) => println!("Activity: {ev:?}"),
            channel::SessionEvent::Message(ev) => {
                if is_reasoning != ev.reasoning {
                    if !is_newline && ev.reasoning {
                        println!();
                    }
                    match ev.reasoning {
                        true => print!("<reasoning>"),
                        false => println!("</reasoning>"),
                    }
                    is_reasoning = ev.reasoning;
                }
                print!("{}", ev.text);
                is_newline = ev.text.ends_with('\n');
            }
            channel::SessionEvent::Permission(ev) => println!("Permission: {ev:?}"),
            channel::SessionEvent::Finished(_) => {}
        }
    }
    println!();
    Ok(())
}
