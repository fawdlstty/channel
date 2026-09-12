//! Requires the `llm` feature:
//! `cargo run --features llm --example llm_chat`
//!
//! Environment variables:
//! - `LLM_BASE_URL` (default `https://api.openai.com`)
//! - `LLM_API_KEY`  (optional for keyless gateways)
//! - `LLM_MODEL`    (default `gpt-4o-mini`)

#[cfg(not(feature = "llm"))]
fn main() {
    eprintln!("this example requires the `llm` feature: cargo run --features llm --example llm_chat");
}

#[cfg(feature = "llm")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base_url =
        std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".to_owned());
    let api_key = std::env::var("LLM_API_KEY").ok();
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned());

    let mut client = channel::ChatCompletionsClient::new(base_url, api_key);
    client.set_system_prompt("You are a concise assistant.");
    client.set_model(model).await?;
    println!("model: {}", client.model().unwrap_or_default());

    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Explain what an SSE stream is in one sentence.".to_owned());

    // Streaming round: print each increment as it arrives.
    let mut receiver = client.chat_stream(prompt).await?;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => print!("{text}"),
            channel::StreamChunk::Error(message) => eprintln!("\n[error: {message}]"),
            channel::StreamChunk::Done => println!(),
        }
    }
    Ok(())
}
