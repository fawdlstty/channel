//! Requires the `local-safetensors-cpu` feature (GGUF models additionally need
//! `local-gguf-cpu`):
//! `cargo run --features local-safetensors-cpu --example local_chat -- ./Qwen3-0.6B`
//!
//! Arguments:
//! - model path: a `*.gguf` file, a `*.safetensors` file or a model
//!   directory with `config.json` + weights (default from `LOCAL_MODEL`)

#[cfg(not(feature = "local-safetensors-cpu"))]
fn main() {
    eprintln!(
        "this example requires the `local-safetensors-cpu` feature: \
         cargo run --features local-safetensors-cpu --example local_chat -- ./Qwen3-0.6B"
    );
}

#[cfg(feature = "local-safetensors-cpu")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model_path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("LOCAL_MODEL").ok())
        .unwrap_or_else(|| "./Qwen3-0.6B".to_owned());

    let mut client = match channel::LocalClient::load(&model_path).await {
        Ok(client) => client,
        Err(channel::Error::UnsupportedCapability(message)) => {
            eprintln!("{message}");
            eprintln!("hint: GGUF models need `--features local-gguf-cpu` (cmake + C++ toolchain)");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let meta = client.meta();
    println!(
        "model: {} (backend {:?}, arch {}, quant {:?}, ctx {})",
        client.model().unwrap_or_default(),
        meta.backend,
        meta.architecture,
        meta.quantization,
        meta.context_length
    );

    client.set_system_prompt("You are a concise assistant.");
    let prompt = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "Explain what an SSE stream is in one sentence.".to_owned());

    // Non-streaming round.
    let reply = client.chat(&prompt).await?;
    println!("reply: {reply}");

    // Streaming round: print each increment as it arrives.
    let mut receiver = client.chat_stream("Now repeat it as a haiku.").await?;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => print!("{text}"),
            channel::StreamChunk::Error(message) => eprintln!("\n[error: {message}]"),
            channel::StreamChunk::Done => println!(),
        }
    }
    Ok(())
}
