#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let harness = channel::Harness::initialize(channel::HarnessKind::Codex).await?;
    let mut config = channel::SessionConfig::for_kind(harness.kind());
    config.set_workspace(Some(std::env::current_dir()?));
    config.set_observability(false);
    // cc-switch 切换是 `ccswitch` feature 的能力；默认构建下跳过配置，
    // 演示保持直连 Codex 可跑。
    #[cfg(feature = "ccswitch")]
    config.set_switch_provider(channel::SwitchProvider::CcSwitch, "deepkey2");
    let mut session = channel::Session::create(config, "你最新的知识库到什么时间").await?;
    let mut is_reasoning = false;
    let mut is_newline = true;
    while let Some(ev) = session.wait_event().await? {
        match ev {
            channel::SessionEvent::Activity(_) => {
                //println!("{:?} {:?}", ev.kind, ev.status);
                if !is_newline {
                    println!();
                    is_newline = true;
                }
            }
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
            channel::SessionEvent::Permission(_) => {}
            channel::SessionEvent::Finished(_) => {}
        }
    }
    println!();
    Ok(())
}
