//! Protocol loopback tests for the local-model HTTP server (design §7.2):
//! the local server is exercised with channel's own protocol clients in the
//! same process. Only runs with the `local-safetensors-cpu` feature (plus `llm`
//! for the client side) and a real model via `CHANNEL_LOCAL_TEST_MODEL`:
//!
//! ```text
//! CHANNEL_LOCAL_TEST_MODEL=./Qwen3-0.6B \
//!   cargo test --features local-safetensors-cpu,llm --test local_server
//! ```

#![cfg(all(feature = "local-safetensors-cpu", feature = "llm"))]

use std::time::Duration;

fn test_model() -> Option<std::path::PathBuf> {
    std::env::var("CHANNEL_LOCAL_TEST_MODEL")
        .ok()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
}

/// Binds a server with the test model mounted on a free high port and
/// waits until it accepts connections.
async fn start_server() -> (String, tokio::task::JoinHandle<()>) {
    let path = test_model().expect("CHANNEL_LOCAL_TEST_MODEL must point at a model");
    for _ in 0..5 {
        let port = pick_port();
        let mut server =
            channel::LocalLlmServer::bind(format!("127.0.0.1:{port}")).expect("bind");
        server.mount_model(&path).expect("model loads");
        let handle = tokio::spawn(async move {
            let _ = server.serve().await;
        });
        let base = format!("http://127.0.0.1:{port}");
        // Wait for the listener; on the rare port collision, retry.
        let mut listening = false;
        for _ in 0..150 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                listening = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if listening {
            return (base, handle);
        }
        handle.abort();
    }
    panic!("the local server never started listening");
}

/// A free TCP port. Racy in theory; retries in `start_server` cover it.
fn pick_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn short_params() -> channel::GenerationParams {
    channel::GenerationParams {
        max_tokens: 24,
        seed: Some(42),
        ..channel::GenerationParams::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_completions_round_trip() {
    let Some(path) = test_model() else {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    };
    let _ = path;
    let (base, server) = start_server().await;

    let mut client = channel::ChatCompletionsClient::new(base.clone(), None);
    client.set_model("whatever").await.expect("single mounted model serves any name");
    let reply = client.chat("Say hi.").await.expect("chat works");
    assert!(!reply.trim().is_empty(), "non-empty reply, got: {reply:?}");

    // Streaming round: SSE frames must arrive incrementally and terminate.
    let mut receiver = client.chat_stream("Count to three.").await.expect("stream works");
    let mut content = String::new();
    let mut saw_done = false;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => content.push_str(&text),
            channel::StreamChunk::Done => {
                saw_done = true;
                break;
            }
            channel::StreamChunk::Error(message) => panic!("stream error: {message}"),
        }
    }
    assert!(saw_done, "stream ends with Done");
    assert!(!content.trim().is_empty(), "stream carries content");
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ollama_chat_round_trip() {
    if test_model().is_none() {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    }
    let (base, server) = start_server().await;

    let mut client = channel::OllamaClient::new(base.clone(), None);
    client.set_model("m").await.expect("model set");
    let reply = client.chat("Say hi.").await.expect("ollama chat works");
    assert!(!reply.trim().is_empty());

    let mut receiver = client.chat_stream("Count to two.").await.expect("stream works");
    let mut content = String::new();
    let mut saw_done = false;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => content.push_str(&text),
            channel::StreamChunk::Done => {
                saw_done = true;
                break;
            }
            channel::StreamChunk::Error(message) => panic!("stream error: {message}"),
        }
    }
    assert!(saw_done);
    assert!(!content.trim().is_empty());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_list_endpoints() {
    if test_model().is_none() {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    }
    let (base, server) = start_server().await;

    let mut openai = channel::ChatCompletionsClient::new(base.clone(), None);
    let models = openai.list_models().await.expect("openai model list");
    assert_eq!(models.len(), 1);

    let mut ollama = channel::OllamaClient::new(base.clone(), None);
    let tags = ollama.list_models().await.expect("ollama tags");
    assert_eq!(tags.len(), 1);
    assert_eq!(models[0].id, tags[0].id);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_and_responses_round_trip() {
    if test_model().is_none() {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    }
    let (base, server) = start_server().await;

    // Anthropic Messages: non-stream and stream.
    let mut messages_client = channel::MessagesClient::new(base.clone(), None);
    messages_client.set_model("m").await.expect("model set");
    let reply = messages_client.chat("Say hi.").await.expect("anthropic chat");
    assert!(!reply.trim().is_empty());
    let mut receiver = messages_client
        .chat_stream("Count to two.")
        .await
        .expect("anthropic stream");
    let mut content = String::new();
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => content.push_str(&text),
            channel::StreamChunk::Done => break,
            channel::StreamChunk::Error(message) => panic!("stream error: {message}"),
        }
    }
    assert!(!content.trim().is_empty());

    // OpenAI Responses: non-stream and stream.
    let mut responses_client = channel::ResponsesClient::new(base.clone(), None);
    responses_client.set_model("m").await.expect("model set");
    let reply = responses_client.chat("Say hi.").await.expect("responses chat");
    assert!(!reply.trim().is_empty());
    let mut receiver = responses_client
        .chat_stream("Count to two.")
        .await
        .expect("responses stream");
    let mut content = String::new();
    let mut saw_done = false;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => content.push_str(&text),
            channel::StreamChunk::Done => {
                saw_done = true;
                break;
            }
            channel::StreamChunk::Error(message) => panic!("stream error: {message}"),
        }
    }
    assert!(saw_done);
    assert!(!content.trim().is_empty());
    server.abort();
}

/// Busy semantics across protocols: while one streaming request holds the
/// model, a second concurrent request gets an HTTP error (not a hang).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_requests_fail_fast() {
    if test_model().is_none() {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    }
    let (base, server) = start_server().await;

    let mut first = channel::ChatCompletionsClient::new(base.clone(), None);
    first.set_model("m").await.expect("model set");
    let mut held = first
        .chat_stream("Write a very long story about a robot.")
        .await
        .expect("first stream");
    // Drain the first stream in the background while the second request
    // runs.
    let drainer = tokio::spawn(async move {
        while let Some(chunk) = held.recv().await {
            if matches!(chunk, channel::StreamChunk::Done) {
                break;
            }
        }
    });
    let mut second = channel::ChatCompletionsClient::new(base.clone(), None);
    second.set_model("m").await.expect("model set");
    match second.chat("hello").await {
        Ok(_) => {} // the first stream already finished: also fine
        Err(channel::Error::ProviderRejected(message)) => {
            assert!(message.contains("409"), "expected 409, got: {message}");
        }
        Err(error) => panic!("unexpected error: {error:?}"),
    }
    let _ = short_params();
    let _ = drainer.await;
    server.abort();
}
