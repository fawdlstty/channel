//! Integration tests for the local-model client. They only run when the
//! `local-safetensors-cpu` feature is compiled in and `CHANNEL_LOCAL_TEST_MODEL`
//! points at a model file or directory on disk (CI never downloads models):
//!
//! ```text
//! CHANNEL_LOCAL_TEST_MODEL=./Qwen3-0.6B cargo test --features local-safetensors-cpu --test local_model
//! CHANNEL_LOCAL_TEST_MODEL=model-q4_k_m.gguf cargo test --features local-gguf-cpu --test local_model
//! ```
//!
//! GGUF paths without the `local-gguf-cpu` feature assert the feature-missing
//! error instead of running inference.

#![cfg(feature = "local-safetensors-cpu")]

use channel::{Error, GenerationParams, LocalBackendKind, LocalClient};

fn test_model() -> Option<std::path::PathBuf> {
    std::env::var("CHANNEL_LOCAL_TEST_MODEL")
        .ok()
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
}

fn is_gguf(path: &std::path::Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false)
}

async fn loaded_client(path: &std::path::Path) -> LocalClient {
    let mut client = LocalClient::load(path).await.expect("model loads");
    client.set_generation_params(GenerationParams {
        max_tokens: 48,
        seed: Some(42),
        ..GenerationParams::default()
    });
    client
}

#[tokio::test]
async fn loads_generates_and_survives_cancel_and_reload() {
    let Some(path) = test_model() else {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    };

    // GGUF without the llama.cpp backend must fail with a feature hint.
    if is_gguf(&path) && !cfg!(feature = "local-gguf-cpu") {
        let error = LocalClient::load(&path).await.unwrap_err();
        assert!(
            matches!(error, Error::UnsupportedCapability(ref message) if message.contains("local-gguf")),
            "got: {error:?}"
        );
        return;
    }

    let mut client = loaded_client(&path).await;

    // The metadata reflects the dispatch decision.
    let meta = client.meta().clone();
    assert!(!meta.architecture.is_empty());
    assert!(meta.context_length > 0);
    let expected_backend = if is_gguf(&path) {
        LocalBackendKind::LlamaCpp
    } else {
        LocalBackendKind::Candle
    };
    assert_eq!(meta.backend, expected_backend);

    // Deterministic seeding: the same prompt, history and seed produce the
    // same reply across two independent generations. `set_messages` keeps
    // the logged system entry, so the rendered prompt is identical for both
    // rounds.
    client.set_system_prompt("Answer in English. Be brief.");
    let first = client.chat("What is 2 + 2?").await.expect("first reply");
    assert!(!first.is_empty(), "the first reply is empty");
    client.set_messages(vec![]);
    let second = client.chat("What is 2 + 2?").await.expect("second reply");
    assert_eq!(first, second, "a fixed seed must reproduce the reply");

    // Stop sequences cut the reply short and never leak into the output.
    client.set_generation_params(GenerationParams {
        max_tokens: 128,
        seed: Some(7),
        stop: vec!["a".to_owned()],
        ..GenerationParams::default()
    });
    let stopped = client
        .chat("Count from one to ten in words.")
        .await
        .expect("stopped reply");
    assert!(
        !stopped.contains('a'),
        "the stop sequence must truncate: {stopped:?}"
    );

    // Cancelling a stream (dropping the receiver) must not poison the
    // engine: the next generation still completes.
    client.set_generation_params(GenerationParams {
        max_tokens: 512,
        seed: Some(42),
        ..GenerationParams::default()
    });
    let receiver = client
        .chat_stream("Write a long story about a robot.")
        .await
        .expect("stream starts");
    drop(receiver);
    // The engine notices the cancelled consumer within a token or two;
    // until then a new request legitimately reports Busy.
    let mut after_cancel = String::new();
    for _ in 0..3000 {
        match client.chat("Say hello.").await {
            Ok(reply) => {
                after_cancel = reply;
                break;
            }
            Err(Error::Busy) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await
            }
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }
    assert!(!after_cancel.is_empty(), "post-cancel reply is empty");

    // Serialize / deserialize round trip: the history survives the model
    // reload (D5).
    let before = client.messages();
    let serialized = client.serialize().expect("serialize");
    let restored = LocalClient::deserialize(&serialized).expect("deserialize reloads the model");
    assert_eq!(restored.messages(), before);
    assert_eq!(restored.meta().backend, meta.backend);
    assert_eq!(restored.model(), client.model());
    // The restored client is immediately usable.
    let mut restored = restored;
    let reply = restored.chat("Say hi.").await.expect("restored reply");
    assert!(!reply.is_empty());
}

#[tokio::test]
async fn streaming_emits_content_then_done() {
    let Some(path) = test_model() else {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    };
    if is_gguf(&path) && !cfg!(feature = "local-gguf-cpu") {
        return;
    }
    let mut client = loaded_client(&path).await;
    let mut receiver = client
        .chat_stream("Say hello.")
        .await
        .expect("stream starts");
    let mut content = String::new();
    let mut saw_done = false;
    while let Some(chunk) = receiver.recv().await {
        match chunk {
            channel::StreamChunk::Content(text) => content.push_str(&text),
            channel::StreamChunk::Done => {
                saw_done = true;
                break;
            }
            channel::StreamChunk::Error(message) => panic!("unexpected stream error: {message}"),
        }
    }
    assert!(saw_done, "the stream must end with Done");
    assert!(!content.is_empty(), "the stream must carry content");
    // The trailing assistant entry mirrors the streamed text.
    let last = client.messages().last().expect("history non-empty").clone();
    assert_eq!(last.content, content);
}

#[tokio::test]
async fn concurrent_generation_reports_busy() {
    let Some(path) = test_model() else {
        eprintln!("skipped: CHANNEL_LOCAL_TEST_MODEL is not set");
        return;
    };
    if is_gguf(&path) && !cfg!(feature = "local-gguf-cpu") {
        return;
    }
    let mut client = loaded_client(&path).await;
    // A short budget keeps the cancellation lag (the engine may run ahead
    // by one channel buffer before it notices the dropped consumer) well
    // inside the polling window below.
    client.set_generation_params(GenerationParams {
        max_tokens: 32,
        seed: Some(42),
        ..GenerationParams::default()
    });
    let _held = client
        .chat_stream("Write a very long story.")
        .await
        .expect("stream starts");
    // The busy flag is held until the stream finishes; a new generation on
    // any clone must observe it.
    let mut clone = client.clone();
    let error = clone.chat("hello").await.unwrap_err();
    assert!(matches!(error, Error::Busy), "got: {error:?}");
    // Finishing the stream releases the lock.
    drop(_held);
    // Poll patiently: the forwarder task releases the flag once the actor
    // notices the cancelled consumer.
    for _ in 0..3000 {
        match client.chat("hello").await {
            Ok(_) => return,
            Err(Error::Busy) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await
            }
            Err(error) => panic!("unexpected error: {error:?}"),
        }
    }
    panic!("the busy flag was never released");
}

