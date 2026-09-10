//! Requires the `local-server` feature (GGUF models additionally need
//! `local-gguf`):
//! `cargo run --features local-server,local-gguf --example local_server -- ./Qwen3-0.6B`
//!
//! Mounts every model path given on the command line (or `LOCAL_MODEL`) and
//! serves the four protocol endpoints:
//!
//! | Endpoint | Protocol |
//! |---|---|
//! | `GET /v1/models` | OpenAI model list |
//! | `POST /v1/chat/completions` | OpenAI Chat Completions |
//! | `POST /v1/responses` | OpenAI Responses |
//! | `POST /v1/messages` | Anthropic Messages |
//! | `POST /api/chat` | Ollama chat |
//! | `GET /api/tags` | Ollama model list |
//!
//! Environment variables:
//! - `LOCAL_SERVER_ADDR` (default `127.0.0.1:8817`)
//! - `LOCAL_SERVER_TOKEN` (optional bearer token)

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("LOCAL_SERVER_ADDR").unwrap_or_else(|_| "127.0.0.1:8817".to_owned());
    let mut paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        if let Ok(model) = std::env::var("LOCAL_MODEL") {
            paths.push(model);
        }
    }
    if paths.is_empty() {
        eprintln!("usage: local_server <model-path> [more-model-paths...]");
        std::process::exit(2);
    }

    let mut server = channel::LocalLlmServer::bind(addr.clone())?;
    if let Ok(token) = std::env::var("LOCAL_SERVER_TOKEN") {
        server = server.with_token(token);
    }
    for path in paths {
        match server.mount_model(&path) {
            Ok(_) => println!("mounted {path}"),
            Err(channel::Error::UnsupportedCapability(message)) => {
                eprintln!("{message}");
                eprintln!("hint: GGUF models need `--features local-gguf` (cmake + C++ toolchain)");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }

    println!(
        "serving {:?} on http://{addr} (POST /v1/chat/completions, /v1/responses, /v1/messages, /api/chat)",
        server.models()
    );
    server.serve().await?;
    Ok(())
}
