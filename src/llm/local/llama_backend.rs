//! The GGUF backend: llama.cpp via llama-cpp-2 (opt-in `local-gguf-cpu`).
//!
//! `LlamaContext` is `!Send`, so the engine lives exclusively on the actor
//! thread. Loading happens on a blocking thread and the loaded engine is
//! then moved to the actor.

#[cfg(feature = "local-gguf-cpu")]
mod enabled {
    use super::super::engine::{GenerateFinish, LocalEngine};
    use super::super::{
        BackendEngine, EngineBoot, GenerationParams, LoadOptions, LocalBackendKind, LocalModelMeta,
    };
    use crate::protocol::Error;
    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::gguf::GgufContext;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;
    use llama_cpp_2::token::LlamaToken;
    use std::num::NonZeroU32;
    use std::path::{Path, PathBuf};

    /// llama.cpp evaluates prompts in batches; a generous chunk keeps
    /// prefill throughput high without a giant scratch allocation.
    const BATCH_TOKENS: usize = 512;

    /// The repeat-penalty window (llama.cpp's `penalty_last_n`), matching
    /// the ollama default that `GenerationParams` mirrors.
    const PENALTY_LAST_N: i32 = 64;

    /// The GGUF metadata key naming the architecture (`qwen3`, ...).
    const KEY_ARCHITECTURE: &str = "general.architecture";
    /// The GGUF metadata key carrying the Jinja chat template.
    const KEY_CHAT_TEMPLATE: &str = "tokenizer.chat_template";

    pub(crate) struct LlamaEngine {
        // Field order matters: the context borrows the model, so it must
        // drop first (Rust drops fields in declaration order).
        context: llama_cpp_2::context::LlamaContext<'static>,
        model: LlamaModel,
        n_ctx: u32,
        eos_token: LlamaToken,
        architecture: String,
        quantization: Option<String>,
        model_path: PathBuf,
    }

    // SAFETY: the engine is created on a loading thread and consumed on the
    // single dedicated actor thread; the context and model are never
    // touched from two threads at once (the actor thread is the only
    // owner after the move).
    unsafe impl Send for LlamaEngine {}

    /// The process-global llama.cpp backend. `LlamaBackend::init` guards a
    /// process-wide flag (and dropping a handle frees the global backend),
    /// so every engine shares one instance for the lifetime of the process.
    fn llama_backend_handle() -> &'static LlamaBackend {
        static BACKEND: std::sync::OnceLock<LlamaBackend> = std::sync::OnceLock::new();
        BACKEND.get_or_init(|| {
            LlamaBackend::init().expect("initializing the llama.cpp backend failed")
        })
    }

    fn map_load_error(error: impl std::fmt::Display, path: &Path) -> Error {
        Error::Backend(format!(
            "llama.cpp failed to load {}: {error}",
            path.display()
        ))
    }

    /// Best-effort quantization label from the file name (`...-Q4_K_M.gguf`
    /// → `Q4_K_M`); GGUF metadata does not carry the quant name.
    pub(super) fn quantization_from_filename(path: &Path) -> Option<String> {
        let stem = path.file_stem()?.to_str()?;
        let tokens: Vec<String> = stem
            .split(['-', '.'])
            .map(|token| token.to_ascii_uppercase())
            .collect();
        for (index, token) in tokens.iter().enumerate() {
            if matches!(token.as_str(), "F16" | "BF16" | "F32") {
                return Some(token.clone());
            }
            // A complete grouped label inside one token: `Q4_K_M`, `Q8_0`,
            // `IQ4_XS`, ...
            if is_quant_grouped(token) {
                return Some(token.clone());
            }
            // A bare head (`Q4`, `IQ4`) picks up the following short
            // alphanumeric groups: `iq4-xs-00001-of-00002` → `IQ4_XS`.
            if is_quant_head(token) {
                let mut label = token.clone();
                for next in tokens.iter().skip(index + 1) {
                    let is_group = !next.is_empty()
                        && next.len() <= 4
                        && next.chars().all(|c| c.is_ascii_alphanumeric())
                        && !next.bytes().all(|b| b.is_ascii_digit());
                    if !is_group {
                        break;
                    }
                    label.push('_');
                    label.push_str(next);
                }
                return Some(label);
            }
        }
        None
    }

    /// `Q`/`IQ` followed by nothing but digits: a quant label head.
    fn is_quant_head(token: &str) -> bool {
        let digits = token.strip_prefix("IQ").or_else(|| token.strip_prefix('Q'));
        match digits {
            Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
            None => false,
        }
    }

    /// A full quant label with its quality groups: `Q4_K_M`, `Q8_0`, ...
    fn is_quant_grouped(token: &str) -> bool {
        token.contains('_')
            && (token.starts_with('Q') || token.starts_with("IQ"))
            && token[1..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && token.chars().any(|c| c.is_ascii_digit())
    }

    /// Reads a string-valued GGUF metadata key.
    fn gguf_string(path: &Path, key: &str) -> Option<String> {
        let gguf = GgufContext::from_file(path)?;
        let index = gguf.find_key(key);
        if index < 0 {
            return None;
        }
        gguf.val_str(index).map(str::to_owned)
    }

    fn piece(model: &LlamaModel, token: LlamaToken) -> Option<String> {
        model
            .token_to_piece_bytes(token, 16, true, None)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    }

    /// Loads a GGUF file (or the first shard of a sharded set) into a boot
    /// record. Must run on a blocking thread.
    pub(crate) fn load_boot(
        gguf_path: &Path,
        original_path: &Path,
        options: &LoadOptions,
    ) -> Result<EngineBoot, Error> {
        let architecture = gguf_string(gguf_path, KEY_ARCHITECTURE).ok_or_else(|| {
            Error::Backend(format!(
                "GGUF file {} has no {KEY_ARCHITECTURE} metadata; the architecture is unknown",
                gguf_path.display()
            ))
        })?;
        let chat_template = gguf_string(gguf_path, KEY_CHAT_TEMPLATE);

        let backend = llama_backend_handle();
        let mut model_params = LlamaModelParams::default().with_use_mmap(options.use_mmap);
        if let Some(layers) = options.n_gpu_layers {
            model_params = model_params.with_n_gpu_layers(layers);
        }
        let model =
            LlamaModel::load_from_file(backend, gguf_path, &model_params).map_err(|error| {
                // llama.cpp rejects unsupported architectures and corrupted
                // weights here.
                map_load_error(error, gguf_path)
            })?;

        let mut context_params =
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(options.n_ctx.max(1)));
        if let Some(threads) = options.n_threads {
            context_params = context_params
                .with_n_threads(threads as i32)
                .with_n_threads_batch(threads as i32);
        }
        let context = model
            .new_context(backend, context_params)
            .map_err(|error| map_load_error(error, gguf_path))?;

        let n_ctx = context.n_ctx();
        let eos_token = model.token_eos();
        let bos_piece = piece(&model, model.token_bos());
        let eos_piece = piece(&model, eos_token);
        let quantization = quantization_from_filename(gguf_path);

        // SAFETY: the context borrows `model`; both live in the returned
        // engine with `context` declared before `model` (dropped first), so
        // the borrow can never be invalidated while the context is alive.
        let context: llama_cpp_2::context::LlamaContext<'static> =
            unsafe { std::mem::transmute(context) };

        let meta = LocalModelMeta {
            path: original_path.to_path_buf(),
            backend: LocalBackendKind::LlamaCpp,
            architecture: architecture.clone(),
            quantization,
            context_length: n_ctx,
        };

        Ok(EngineBoot {
            engine: BackendEngine::LlamaCpp(LlamaEngine {
                context,
                model,
                n_ctx,
                eos_token,
                architecture,
                quantization: meta.quantization.clone(),
                model_path: original_path.to_path_buf(),
            }),
            meta,
            template: chat_template,
            bos_token: bos_piece,
            eos_token: eos_piece,
            n_ctx,
        })
    }

    /// Converts a raw byte increment into the longest complete UTF-8
    /// prefix, keeping the incomplete tail buffered for the next token.
    fn drain_complete_utf8(buffer: &mut Vec<u8>) -> String {
        let valid = match std::str::from_utf8(buffer) {
            Ok(_) => buffer.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid == 0 {
            return String::new();
        }
        // The valid prefix is guaranteed UTF-8, so the lossy conversion is
        // lossless in practice.
        let text = String::from_utf8_lossy(&buffer[..valid]).into_owned();
        buffer.drain(..valid);
        text
    }

    impl LocalEngine for LlamaEngine {
        fn load(path: &Path, options: &LoadOptions) -> Result<Self, Error> {
            load_boot(path, path, options).map(|boot| match boot.engine {
                BackendEngine::LlamaCpp(engine) => engine,
                #[allow(unreachable_patterns)]
                _ => unreachable!("the llama boot only produces llama engines"),
            })
        }

        fn meta(&self) -> LocalModelMeta {
            LocalModelMeta {
                path: self.model_path.clone(),
                backend: LocalBackendKind::LlamaCpp,
                architecture: self.architecture.clone(),
                quantization: self.quantization.clone(),
                context_length: self.n_ctx,
            }
        }

        fn tokenize(&self, text: &str) -> Result<Vec<u32>, Error> {
            // Special tokens embedded in the rendered template (for example
            // `<|im_start|>`) must map back to their vocabulary ids, which
            // is what llama.cpp's `special = true` tokenization does.
            let tokens = self
                .model
                .str_to_token(text, AddBos::Never)
                .map_err(|error| Error::Backend(format!("tokenization failed: {error}")))?;
            Ok(tokens.into_iter().map(|token| token.0 as u32).collect())
        }

        fn generate(
            &mut self,
            prompt_tokens: &[u32],
            params: &GenerationParams,
            emit: &mut dyn FnMut(&str) -> bool,
        ) -> Result<GenerateFinish, Error> {
            let n_ctx = self.context.n_ctx() as usize;
            if prompt_tokens.is_empty() {
                return Err(Error::InvalidConfig(
                    "the rendered prompt produced no tokens".to_owned(),
                ));
            }
            if prompt_tokens.len() >= n_ctx {
                return Err(Error::InvalidConfig(format!(
                    "the prompt ({} tokens) does not fit the context window ({}); \
                     increase LoadOptions::n_ctx or shorten the conversation",
                    prompt_tokens.len(),
                    n_ctx
                )));
            }
            let max_new = params.max_tokens.min((n_ctx - prompt_tokens.len()) as u32);

            let seed = params.seed.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.subsec_nanos())
                    .unwrap_or(0)
            });
            // The chain order follows llama.cpp's defaults: penalties,
            // top_k, top_p, temperature, then the final sampler. A near-zero
            // temperature degenerates to greedy sampling.
            let mut samplers = vec![
                LlamaSampler::penalties(
                    self.model.n_vocab(),
                    PENALTY_LAST_N,
                    params.repeat_penalty,
                    0.0,
                    0.0,
                ),
                LlamaSampler::top_k(params.top_k),
                LlamaSampler::top_p(params.top_p as f32, 1),
            ];
            if params.temperature < 1e-7 {
                samplers.push(LlamaSampler::greedy());
            } else {
                samplers.push(LlamaSampler::temp(params.temperature as f32));
                samplers.push(LlamaSampler::dist(seed));
            }
            let mut sampler = LlamaSampler::chain(samplers, true).with_tokens(
                prompt_tokens
                    .iter()
                    .map(|token| LlamaToken(*token as i32))
                    .collect::<Vec<_>>(),
            );

            // Fresh KV cache for every generation.
            self.context.clear_kv_cache();

            // Pre-fill in batches.
            let mut n_past: i32 = 0;
            let mut index = 0usize;
            while index < prompt_tokens.len() {
                let end = (index + BATCH_TOKENS).min(prompt_tokens.len());
                let chunk = &prompt_tokens[index..end];
                let mut batch = LlamaBatch::new(chunk.len(), 1);
                for (position, token) in chunk.iter().enumerate() {
                    batch
                        .add(
                            LlamaToken(*token as i32),
                            n_past + position as i32,
                            &[0],
                            position == chunk.len() - 1,
                        )
                        .map_err(|error| Error::Backend(format!("batch build failed: {error}")))?;
                }
                self.context
                    .decode(&mut batch)
                    .map_err(|error| Error::Backend(format!("prefill decode failed: {error}")))?;
                n_past += chunk.len() as i32;
                index = end;
            }

            let mut generated = 0u32;
            let mut utf8_buffer: Vec<u8> = Vec::new();
            let mut finish = GenerateFinish::Length;

            loop {
                // idx -1 reads the logits of last decode's (only) output;
                // non-negative indices are batch positions in current
                // llama.cpp, not output ordinals.
                let token = sampler.sample(&self.context, -1);
                if token == self.eos_token {
                    finish = GenerateFinish::Eos;
                    break;
                }
                if let Ok(bytes) = self.model.token_to_piece_bytes(token, 16, false, None) {
                    utf8_buffer.extend_from_slice(&bytes);
                    let text = drain_complete_utf8(&mut utf8_buffer);
                    if !text.is_empty() && !emit(&text) {
                        finish = GenerateFinish::Cancelled;
                        break;
                    }
                }
                sampler.accept(token);
                generated += 1;
                if generated >= max_new {
                    // `finish` already holds `GenerateFinish::Length`.
                    break;
                }

                let mut batch = LlamaBatch::new(1, 1);
                batch
                    .add(token, n_past, &[0], true)
                    .map_err(|error| Error::Backend(format!("batch build failed: {error}")))?;
                self.context
                    .decode(&mut batch)
                    .map_err(|error| Error::Backend(format!("decode failed: {error}")))?;
                n_past += 1;
            }

            Ok(finish)
        }

        fn reset(&mut self) -> Result<(), Error> {
            self.context.clear_kv_cache();
            Ok(())
        }
    }
}

#[cfg(feature = "local-gguf-cpu")]
pub(crate) use enabled::{load_boot, LlamaEngine};

#[cfg(all(test, feature = "local-gguf-cpu"))]
mod tests {
    use super::enabled::quantization_from_filename;
    use std::path::Path;

    #[test]
    fn quantization_label_comes_from_the_filename() {
        assert_eq!(
            quantization_from_filename(Path::new("/m/Qwen3-0.6B-Q4_K_M.gguf")),
            Some("Q4_K_M".to_owned())
        );
        assert_eq!(
            quantization_from_filename(Path::new("/m/qwen2.5-iq4-xs-00001-of-00002.gguf")),
            Some("IQ4_XS".to_owned())
        );
        assert_eq!(
            quantization_from_filename(Path::new("/m/chat-q8_0.gguf")),
            Some("Q8_0".to_owned())
        );
        assert_eq!(
            quantization_from_filename(Path::new("/m/model-f16.gguf")),
            Some("F16".to_owned())
        );
        assert_eq!(quantization_from_filename(Path::new("/m/plain.gguf")), None);
    }
}

#[cfg(not(feature = "local-gguf-cpu"))]
use super::{EngineBoot, LoadOptions};

/// Placeholder backend: produces the feature-missing error when GGUF
/// support was compiled out.
#[cfg(not(feature = "local-gguf-cpu"))]
pub(crate) fn load_boot(
    _gguf_path: &std::path::Path,
    _original_path: &std::path::Path,
    _options: &LoadOptions,
) -> Result<EngineBoot, crate::protocol::Error> {
    Err(crate::protocol::Error::UnsupportedCapability(
        "GGUF models run on llama.cpp, which is not compiled in: enable the `local-gguf-cpu` \
         feature (requires cmake and a C++ toolchain)"
            .to_owned(),
    ))
}
