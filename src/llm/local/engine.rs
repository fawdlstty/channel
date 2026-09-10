//! The inference actor: a dedicated OS thread that owns the engine.
//!
//! Both backends need exclusive, blocking access to their model state
//! (llama.cpp's `LlamaContext` is `!Send`, candle decode is CPU bound), so
//! every engine lives on its own thread behind an mpsc channel. The async
//! side only holds channel handles, so the tokio runtime is never blocked.

use super::{GenerationParams, LoadOptions, LocalModelMeta};
use crate::llm::StreamChunk;
use crate::protocol::Error;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// Why a generation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GenerateFinish {
    /// The model produced its end-of-sequence token.
    Eos,
    /// The token budget (`max_tokens`, bounded by the context window) was
    /// reached.
    Length,
    /// A stop sequence from [`GenerationParams::stop`] matched.
    StopSequence,
    /// The `emit` callback asked to stop (the consumer went away or a stop
    /// sequence matched upstream of the engine).
    Cancelled,
}

/// The backend-agnostic inference interface implemented by the candle and
/// llama.cpp backends.
///
/// All methods except [`LocalEngine::load`] run on the dedicated actor
/// thread. [`LocalEngine::load`] is heavy (it maps the weights) and is meant
/// to run inside `spawn_blocking`.
pub(crate) trait LocalEngine: Send + 'static {
    /// Loads the engine identified by the dispatch decision.
    // Kept for interface parity with the design; `load_boot` on the backend
    // modules is the entry actually used (it also carries the template).
    #[allow(dead_code)]
    fn load(path: &Path, options: &LoadOptions) -> Result<Self, Error>
    where
        Self: Sized;

    /// The model metadata, computed right after loading.
    // `EngineBoot::meta` carries the metadata to the client instead.
    #[allow(dead_code)]
    fn meta(&self) -> LocalModelMeta;

    /// Tokenizes `text` with the model's own vocabulary. Lives behind the
    /// actor because llama.cpp vocabularies cannot leave the engine thread.
    fn tokenize(&self, text: &str) -> Result<Vec<u32>, Error>;

    /// Generates tokens after `prompt_tokens`, calling `emit` with decoded
    /// text increments. Returning `false` from `emit` stops the generation
    /// (the result is then [`GenerateFinish::Cancelled`]).
    fn generate(
        &mut self,
        prompt_tokens: &[u32],
        params: &GenerationParams,
        emit: &mut dyn FnMut(&str) -> bool,
    ) -> Result<GenerateFinish, Error>;

    /// Drops the decode state (llama.cpp: clears the KV cache, candle:
    /// drops the kv cache) so the next generation starts fresh. Called
    /// after a cancelled generation.
    fn reset(&mut self) -> Result<(), Error>;
}

/// The messages exchanged with the actor thread.
pub(crate) enum EngineRequest {
    Generate {
        prompt_tokens: Vec<u32>,
        params: GenerationParams,
        emit: tokio::sync::mpsc::Sender<StreamChunk>,
    },
    /// Internal extension over the design's enum: exact token counting for
    /// the context-overflow truncation needs the engine's vocabulary, and
    /// llama.cpp vocabularies cannot be used off the actor thread.
    Tokenize {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<Vec<u32>, Error>>,
    },
    Shutdown,
}

/// The handle the async side talks to. Cheap to clone via [`Arc`].
pub(crate) struct EngineActor {
    tx: Arc<Sender<EngineRequest>>,
    panicked: Arc<AtomicBool>,
}

impl EngineActor {
    /// Moves `engine` onto a dedicated OS thread and returns the handle.
    pub(crate) fn spawn<E: LocalEngine>(engine: E) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<EngineRequest>();
        let panicked = Arc::new(AtomicBool::new(false));
        let thread_flag = panicked.clone();
        std::thread::Builder::new()
            .name("channel-local-engine".to_owned())
            .spawn(move || run_actor(engine, rx, thread_flag))
            .expect("spawning the inference thread failed");
        Self {
            tx: Arc::new(tx),
            panicked,
        }
    }

    fn poison_error(&self) -> Error {
        if self.panicked.load(Ordering::Acquire) {
            Error::Backend("inference thread panicked; reload the model".to_owned())
        } else {
            Error::Closed
        }
    }

    /// Fails fast once the actor thread has panicked.
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.panicked.load(Ordering::Acquire) {
            return Err(Error::Backend(
                "inference thread panicked; reload the model".to_owned(),
            ));
        }
        Ok(())
    }

    /// Tokenizes on the actor thread.
    pub(crate) async fn tokenize(&self, text: &str) -> Result<Vec<u32>, Error> {
        self.check()?;
        let (reply, received) = tokio::sync::oneshot::channel();
        self.tx
            .send(EngineRequest::Tokenize {
                text: text.to_owned(),
                reply,
            })
            .map_err(|_| self.poison_error())?;
        received.await.map_err(|_| self.poison_error())?
    }

    /// Queues a generation; the actor streams [`StreamChunk`]s into `emit`.
    pub(crate) fn generate(
        &self,
        prompt_tokens: Vec<u32>,
        params: GenerationParams,
        emit: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<(), Error> {
        self.check()?;
        self.tx
            .send(EngineRequest::Generate {
                prompt_tokens,
                params,
                emit,
            })
            .map_err(|_| self.poison_error())
    }
}

impl Drop for EngineActor {
    fn drop(&mut self) {
        // The thread finishes the in-flight request first, then exits.
        let _ = self.tx.send(EngineRequest::Shutdown);
    }
}

fn run_actor<E: LocalEngine>(
    mut engine: E,
    rx: Receiver<EngineRequest>,
    panicked: Arc<AtomicBool>,
) {
    while let Ok(request) = rx.recv() {
        match request {
            EngineRequest::Tokenize { text, reply } => {
                let _ = reply.send(engine.tokenize(&text));
            }
            EngineRequest::Generate {
                prompt_tokens,
                params,
                emit,
            } => {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run_generate(&mut engine, &prompt_tokens, &params, &emit)
                }));
                match outcome {
                    Ok(Ok(_)) => {
                        let _ = emit.blocking_send(StreamChunk::Done);
                    }
                    Ok(Err(error)) => {
                        let _ = emit.blocking_send(StreamChunk::Error(error.to_string()));
                        let _ = emit.blocking_send(StreamChunk::Done);
                    }
                    Err(_panic) => {
                        // The engine state is unknown; refuse everything
                        // from now on. The model has to be reloaded.
                        panicked.store(true, Ordering::Release);
                        let _ = emit.blocking_send(StreamChunk::Error(
                            "inference thread panicked; reload the model".to_owned(),
                        ));
                        let _ = emit.blocking_send(StreamChunk::Done);
                        break;
                    }
                }
            }
            EngineRequest::Shutdown => break,
        }
    }
}

/// Drives one generation, applying stop-sequence matching on the decoded
/// text stream (so the logic is shared by both backends).
fn run_generate(
    engine: &mut impl LocalEngine,
    prompt_tokens: &[u32],
    params: &GenerationParams,
    emit: &tokio::sync::mpsc::Sender<StreamChunk>,
) -> Result<GenerateFinish, Error> {
    let mut matcher = StopMatcher::new(&params.stop);
    let mut stop_hit = false;
    let mut consumer_alive = true;

    let result = engine.generate(prompt_tokens, params, &mut |delta| {
        if !consumer_alive {
            return false;
        }
        match matcher.push(delta) {
            StopOutcome::Emit(text) => {
                if text.is_empty() {
                    return true;
                }
                if emit.blocking_send(StreamChunk::Content(text)).is_ok() {
                    true
                } else {
                    consumer_alive = false;
                    false
                }
            }
            StopOutcome::Stopped(tail) => {
                stop_hit = true;
                if !tail.is_empty() {
                    let _ = emit.blocking_send(StreamChunk::Content(tail));
                }
                false
            }
        }
    });

    match result {
        Ok(GenerateFinish::Cancelled) if stop_hit => Ok(GenerateFinish::StopSequence),
        Ok(GenerateFinish::Cancelled) => {
            // The consumer went away; drop the decode state so the next
            // generation starts from a clean slate.
            engine.reset()?;
            Ok(GenerateFinish::Cancelled)
        }
        Ok(finish) => {
            // Natural end: release the text held back as a potential stop
            // prefix.
            let tail = matcher.finish();
            if !tail.is_empty() {
                let _ = emit.blocking_send(StreamChunk::Content(tail));
            }
            Ok(finish)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
enum StopOutcome {
    /// Text that can never be part of a stop match; forward it (may be
    /// empty when everything is held back).
    Emit(String),
    /// A stop sequence matched: `tail` is the text before it, to be
    /// forwarded once, and generation must end. The stop sequence itself
    /// is never emitted.
    Stopped(String),
}

/// Incremental stop-sequence matcher over decoded text.
///
/// Text that could still grow into a stop sequence (a suffix of the buffer
/// that prefixes some stop) is held back until the ambiguity resolves.
struct StopMatcher {
    stops: Vec<String>,
    pending: String,
}

impl StopMatcher {
    fn new(stops: &[String]) -> Self {
        Self {
            stops: stops.iter().filter(|s| !s.is_empty()).cloned().collect(),
            pending: String::new(),
        }
    }

    fn push(&mut self, delta: &str) -> StopOutcome {
        self.pending.push_str(delta);
        for stop in &self.stops {
            if let Some(position) = self.pending.find(stop.as_str()) {
                let tail = self.pending[..position].to_owned();
                self.pending.clear();
                return StopOutcome::Stopped(tail);
            }
        }
        // Hold back the longest suffix that is a strict prefix of a stop
        // sequence; everything before it is safe to emit. Byte lengths that
        // would slice a stop sequence inside a multi-byte character are
        // skipped.
        let mut hold_back = 0usize;
        for stop in &self.stops {
            let max = self.pending.len().min(stop.len().saturating_sub(1));
            for length in (1..=max).rev() {
                if !stop.is_char_boundary(length) {
                    continue;
                }
                if self.pending.ends_with(&stop[..length]) {
                    hold_back = hold_back.max(length);
                    break;
                }
            }
        }
        let safe_length = self.pending.len() - hold_back;
        let emit = self.pending[..safe_length].to_owned();
        self.pending.drain(..safe_length);
        StopOutcome::Emit(emit)
    }

    /// Flushes the held-back text once generation ended without a match.
    fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(stops: &[&str]) -> StopMatcher {
        StopMatcher::new(
            &stops
                .iter()
                .map(|stop| stop.to_string())
                .collect::<Vec<_>>(),
        )
    }

    fn collect(stops: &[&str], deltas: &[&str]) -> (String, bool) {
        let mut m = matcher(stops);
        let mut out = String::new();
        let mut stopped = false;
        for delta in deltas {
            match m.push(delta) {
                StopOutcome::Emit(text) => out.push_str(&text),
                StopOutcome::Stopped(tail) => {
                    out.push_str(&tail);
                    stopped = true;
                    break;
                }
            }
        }
        if !stopped {
            out.push_str(&m.finish());
        }
        (out, stopped)
    }

    #[test]
    fn stop_sequence_within_one_delta() {
        let (out, stopped) = collect(&["STOP"], &["hello STOP world"]);
        assert_eq!(out, "hello ");
        assert!(stopped);
    }

    #[test]
    fn stop_sequence_across_deltas_holds_back_prefix() {
        // "ST" is held back (a prefix of "STOP"), then the match completes.
        let (out, stopped) = collect(&["STOP"], &["hello", " ST", "OP more"]);
        assert_eq!(out, "hello ");
        assert!(stopped);
        // The same input without a stop list forwards everything.
        let (out, stopped) = collect(&[], &["hello", " ST", "OP more"]);
        assert_eq!(out, "hello STOP more");
        assert!(!stopped);
    }

    #[test]
    fn partial_prefix_flushes_on_natural_end() {
        let mut m = matcher(&["STOP"]);
        assert!(matches!(m.push("hello ST"), StopOutcome::Emit(text) if text == "hello "));
        assert_eq!(m.finish(), "ST");
        assert_eq!(m.finish(), "");
    }

    #[test]
    fn held_back_prefix_completes_into_a_stop_match() {
        // "a" is held back as a prefix of "ab"; the next delta completes
        // the match. Text emitted before the match is not repeated in the
        // stop tail.
        let mut m = matcher(&["ab", "bc"]);
        match m.push("xxxa") {
            StopOutcome::Emit(text) => assert_eq!(text, "xxx"),
            other => panic!("unexpected: {other:?}"),
        }
        match m.push("b") {
            StopOutcome::Stopped(tail) => assert_eq!(tail, ""),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn multiple_stops_hold_back_the_longest_suffix() {
        // "abc" (a prefix of "abcd") holds back three bytes while "cd"
        // would only hold back one; the longest wins.
        let mut m = matcher(&["abcd", "cd"]);
        match m.push("xxabc") {
            StopOutcome::Emit(text) => assert_eq!(text, "xx"),
            other => panic!("unexpected: {other:?}"),
        }
        match m.push("d") {
            StopOutcome::Stopped(tail) => assert_eq!(tail, ""),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn multibyte_stop_sequence_matches_on_char_boundaries() {
        let (out, stopped) = collect(&["。"], &["好的", "。继续"]);
        assert_eq!(out, "好的");
        assert!(stopped);
    }
}
