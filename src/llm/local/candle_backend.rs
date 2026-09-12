//! The safetensors backend: candle + tokenizers (the pure-Rust `local`
//! baseline).
//!
//! Supported `config.json` `model_type` values: `llama`, `qwen2`, `qwen3`,
//! `phi3`, `gemma`, via the ready-made implementations in
//! candle-transformers 0.11.

use super::engine::{GenerateFinish, LocalEngine};
use super::{
    BackendEngine, EngineBoot, GenerationParams, LoadOptions, LocalBackendKind, LocalModelMeta,
};
use crate::protocol::Error;
use candle_core::{DType, Device, Tensor};
use candle_nn::var_builder::SimpleBackend;
use candle_nn::VarBuilder;
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::{gemma, llama, phi3, qwen2, qwen3};
use candle_transformers::utils::apply_repeat_penalty;
use std::path::{Path, PathBuf};

/// The architectures this backend understands; used for the
/// `UnsupportedCapability` message.
pub(crate) const SUPPORTED_ARCHITECTURES: &str = "llama, qwen2, qwen3, phi3, gemma";

/// Pre-fill chunk size; keeps the attention mask allocation bounded.
const PREFILL_CHUNK: usize = 1024;

/// The per-architecture model wrappers. Every variant's `forward` returns
/// the logits of the last position.
enum CandleModel {
    Llama {
        model: Box<llama::Llama>,
        cache: llama::Cache,
        config: llama::Config,
    },
    Qwen2(qwen2::ModelForCausalLM),
    Qwen3(qwen3::ModelForCausalLM),
    Phi3(phi3::Model),
    Gemma(gemma::Model),
}

impl CandleModel {
    fn forward(&mut self, tokens: &[u32], offset: usize, device: &Device) -> Result<Tensor, Error> {
        let input = Tensor::new(tokens, device)
            .and_then(|tensor| tensor.unsqueeze(0))
            .map_err(candle_error)?;
        let logits = match self {
            Self::Llama { model, cache, .. } => model.forward(&input, offset, cache),
            Self::Qwen2(model) => model.forward(&input, offset),
            Self::Qwen3(model) => model.forward(&input, offset),
            Self::Phi3(model) => model.forward(&input, offset),
            Self::Gemma(model) => model.forward(&input, offset),
        }
        .map_err(candle_error)?;
        let logits = logits.flatten_all().map_err(candle_error)?;
        logits.to_dtype(DType::F32).map_err(candle_error)
    }

    fn clear_kv_cache(&mut self, device: &Device) {
        match self {
            Self::Llama { cache, config, .. } => {
                // llama::Cache has no clear method in candle 0.11; a fresh
                // cache is cheap (it only precomputes the RoPE tables).
                if let Ok(fresh) = llama::Cache::new(true, DType::F32, config, device) {
                    *cache = fresh;
                }
            }
            Self::Qwen2(model) => model.clear_kv_cache(),
            Self::Qwen3(model) => model.clear_kv_cache(),
            Self::Phi3(model) => model.clear_kv_cache(),
            Self::Gemma(model) => model.clear_kv_cache(),
        }
    }
}

pub(crate) struct CandleEngine {
    model: CandleModel,
    device: Device,
    tokenizer: tokenizers::Tokenizer,
    architecture: String,
    max_position_embeddings: usize,
    eos_ids: Vec<u32>,
}

/// A multi-file in-memory safetensors backend for `use_mmap = false`
/// loads (sharded checkpoints cannot go through the single-buffer loader).
struct MultiBufferedSafetensors(Vec<candle_core::safetensors::BufferedSafetensors>);

impl SimpleBackend for MultiBufferedSafetensors {
    fn get(
        &self,
        s: candle_core::Shape,
        name: &str,
        _init: candle_nn::Init,
        dtype: DType,
        dev: &Device,
    ) -> candle_core::Result<Tensor> {
        for shard in &self.0 {
            if shard.get(name).is_ok() {
                let tensor = shard.load(name, dev)?.to_dtype(dtype)?;
                if tensor.shape() != &s {
                    return Err(candle_core::Error::UnexpectedShape {
                        msg: format!("shape mismatch for {name}"),
                        expected: s,
                        got: tensor.shape().clone(),
                    });
                }
                return Ok(tensor);
            }
        }
        candle_core::bail!("tensor {name} not found in any safetensors shard")
    }

    fn get_unchecked(&self, name: &str, dtype: DType, dev: &Device) -> candle_core::Result<Tensor> {
        for shard in &self.0 {
            if let Ok(tensor) = shard.get_unchecked(name, dtype, dev) {
                return Ok(tensor);
            }
        }
        candle_core::bail!("tensor {name} not found in any safetensors shard")
    }

    fn contains_tensor(&self, name: &str) -> bool {
        self.0.iter().any(|shard| shard.get(name).is_ok())
    }
}

fn candle_error(error: candle_core::Error) -> Error {
    Error::Backend(format!("candle backend: {error}"))
}

fn read_json(path: &Path, what: &str) -> Result<serde_json::Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::InvalidConfig(format!("cannot read {what} at {}: {error}", path.display()))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        Error::InvalidConfig(format!("invalid {what} at {}: {error}", path.display()))
    })
}

/// `bos_token`/`eos_token` in `tokenizer_config.json` are either plain
/// strings or `{"content": "..."}` objects.
fn special_token(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(|content| content.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

fn select_device() -> Result<Device, Error> {
    // GPU is only wired up when the corresponding forwarding feature is on.
    #[cfg(feature = "local-safetensors-cuda")]
    return Device::new_cuda(0).map_err(candle_error);
    #[cfg(all(feature = "local-safetensors-metal", not(feature = "local-safetensors-cuda")))]
    return Device::new_metal(0).map_err(candle_error);
    #[cfg(not(any(feature = "local-safetensors-cuda", feature = "local-safetensors-metal")))]
    Ok(Device::Cpu)
}

fn safetensors_files(dir: &Path, single: Option<&Path>) -> Result<Vec<PathBuf>, Error> {
    if let Some(file) = single {
        return Ok(vec![file.to_path_buf()]);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| Error::InvalidConfig(format!("cannot list {}: {error}", dir.display())))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("safetensors")
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(Error::InvalidConfig(format!(
            "no *.safetensors weights found in {}",
            dir.display()
        )));
    }
    Ok(files)
}

fn var_builder(
    files: &[PathBuf],
    options: &LoadOptions,
    device: &Device,
) -> Result<VarBuilder<'static>, Error> {
    if options.use_mmap {
        // SAFETY: the files are mmapped read-only and the mapping outlives
        // the model built from it (both are owned by the engine).
        unsafe { VarBuilder::from_mmaped_safetensors(files, DType::F32, device) }
            .map_err(candle_error)
    } else {
        let shards = files
            .iter()
            .map(|file| {
                let data = std::fs::read(file).map_err(|error| {
                    Error::InvalidConfig(format!("cannot read {}: {error}", file.display()))
                })?;
                candle_core::safetensors::BufferedSafetensors::new(data).map_err(|error| {
                    Error::Backend(format!(
                        "invalid safetensors file {}: {error}",
                        file.display()
                    ))
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(VarBuilder::from_backend(
            Box::new(MultiBufferedSafetensors(shards)),
            DType::F32,
            device.clone(),
        ))
    }
}

/// Loads a safetensors model directory (or a single `*.safetensors` file
/// with its sibling metadata) into a boot record.
pub(crate) fn load_boot(
    dir: &Path,
    single_file: Option<&Path>,
    original_path: &Path,
    options: &LoadOptions,
) -> Result<EngineBoot, Error> {
    let config_value = read_json(&dir.join("config.json"), "model config")?;
    let model_type = config_value
        .get("model_type")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            Error::InvalidConfig(format!(
                "config.json at {} is missing the model_type field",
                dir.display()
            ))
        })?
        .to_owned();

    let tokenizer_path = dir.join("tokenizer.json");
    if !tokenizer_path.is_file() {
        return Err(Error::InvalidConfig(format!(
            "missing tokenizer.json in {} (required by the safetensors backend)",
            dir.display()
        )));
    }
    let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|error| {
        Error::InvalidConfig(format!(
            "cannot load tokenizer.json in {}: {error}",
            dir.display()
        ))
    })?;

    if let Some(threads) = options.n_threads {
        // candle reads this on first use; loads happen before any tensor op.
        std::env::set_var("CANDLE_NUM_THREADS", threads.to_string());
    }

    let device = select_device()?;
    let files = safetensors_files(dir, single_file)?;
    let vb = var_builder(&files, options, &device)?;

    let mut eos_ids: Vec<u32> = Vec::new();
    let (model, max_position_embeddings) = match model_type.as_str() {
        "llama" => {
            let config: llama::LlamaConfig = serde_json::from_value(config_value.clone())
                .map_err(|error| Error::InvalidConfig(format!("invalid llama config: {error}")))?;
            let max_position_embeddings = config.max_position_embeddings;
            match config.eos_token_id {
                Some(llama::LlamaEosToks::Single(id)) => eos_ids.push(id),
                Some(llama::LlamaEosToks::Multiple(ref ids)) => eos_ids.extend(ids.iter().copied()),
                None => {}
            }
            let config = config.into_config(false);
            let model = Box::new(llama::Llama::load(vb, &config).map_err(candle_error)?);
            let cache =
                llama::Cache::new(true, DType::F32, &config, &device).map_err(candle_error)?;
            (
                CandleModel::Llama {
                    model,
                    cache,
                    config,
                },
                max_position_embeddings,
            )
        }
        "qwen2" => {
            let config: qwen2::Config = serde_json::from_value(config_value.clone())
                .map_err(|error| Error::InvalidConfig(format!("invalid qwen2 config: {error}")))?;
            let model = qwen2::ModelForCausalLM::new(&config, vb).map_err(candle_error)?;
            (CandleModel::Qwen2(model), config.max_position_embeddings)
        }
        "qwen3" => {
            let config: qwen3::Config = serde_json::from_value(config_value.clone())
                .map_err(|error| Error::InvalidConfig(format!("invalid qwen3 config: {error}")))?;
            let model = qwen3::ModelForCausalLM::new(&config, vb).map_err(candle_error)?;
            (CandleModel::Qwen3(model), config.max_position_embeddings)
        }
        "phi3" => {
            let config: phi3::Config = serde_json::from_value(config_value.clone())
                .map_err(|error| Error::InvalidConfig(format!("invalid phi3 config: {error}")))?;
            let model = phi3::Model::new(&config, vb).map_err(candle_error)?;
            eos_ids.extend(config.eos_token_id);
            (CandleModel::Phi3(model), config.max_position_embeddings)
        }
        "gemma" => {
            let config: gemma::Config = serde_json::from_value(config_value.clone())
                .map_err(|error| Error::InvalidConfig(format!("invalid gemma config: {error}")))?;
            let model = gemma::Model::new(false, &config, vb).map_err(candle_error)?;
            (CandleModel::Gemma(model), config.max_position_embeddings)
        }
        other => {
            return Err(Error::UnsupportedCapability(format!(
                "architecture '{other}' is not supported by the candle backend \
                 (supported: {SUPPORTED_ARCHITECTURES}); convert the model to GGUF and \
                 enable the `local-gguf-cpu` feature instead"
            )));
        }
    };

    // Chat template and special tokens come from tokenizer_config.json;
    // the end-of-sequence ids of qwen2/qwen3/gemma configs are not modeled
    // by candle, so the tokenizer's vocabulary fills the gap.
    let tokenizer_config = read_json(&dir.join("tokenizer_config.json"), "tokenizer config").ok();
    let chat_template = tokenizer_config
        .as_ref()
        .and_then(|config| config.get("chat_template"))
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    let bos_token = tokenizer_config
        .as_ref()
        .and_then(|config| config.get("bos_token"))
        .and_then(special_token);
    let eos_token = tokenizer_config
        .as_ref()
        .and_then(|config| config.get("eos_token"))
        .and_then(special_token);
    if let Some(id) = eos_token
        .as_deref()
        .and_then(|token| tokenizer.token_to_id(token))
    {
        if !eos_ids.contains(&id) {
            eos_ids.push(id);
        }
    }

    // The effective window: never exceed the model's trained positions.
    let n_ctx = options.n_ctx.min(max_position_embeddings as u32).max(1);

    let meta = LocalModelMeta {
        path: original_path.to_path_buf(),
        backend: LocalBackendKind::Candle,
        architecture: model_type,
        quantization: None,
        context_length: max_position_embeddings as u32,
    };

    Ok(EngineBoot {
        engine: BackendEngine::Candle(Box::new(CandleEngine {
            model,
            device,
            tokenizer,
            architecture: meta.architecture.clone(),
            max_position_embeddings,
            eos_ids,
        })),
        meta,
        template: chat_template,
        bos_token,
        eos_token,
        n_ctx,
    })
}

impl LocalEngine for CandleEngine {
    fn load(path: &Path, options: &LoadOptions) -> Result<Self, Error> {
        // The candle engine is always booted through `load_boot` (which also
        // resolves the chat template); this trait entry point maps the plain
        // path shapes onto it.
        let (dir, single_file) = if path.is_dir() {
            (path.to_path_buf(), None)
        } else {
            (
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
                Some(path),
            )
        };
        match load_boot(&dir, single_file, path, options)?.engine {
            BackendEngine::Candle(engine) => Ok(*engine),
            #[allow(unreachable_patterns)]
            _ => unreachable!("the candle boot only produces candle engines"),
        }
    }

    fn meta(&self) -> LocalModelMeta {
        LocalModelMeta {
            path: PathBuf::new(),
            backend: LocalBackendKind::Candle,
            architecture: self.architecture.clone(),
            quantization: None,
            context_length: self.max_position_embeddings as u32,
        }
    }

    fn tokenize(&self, text: &str) -> Result<Vec<u32>, Error> {
        let encoding = self.tokenizer.encode(text, false).map_err(|error| {
            Error::Backend(format!(
                "tokenization with the {} tokenizer failed: {error}",
                self.architecture
            ))
        })?;
        Ok(encoding.get_ids().to_vec())
    }

    fn generate(
        &mut self,
        prompt_tokens: &[u32],
        params: &GenerationParams,
        emit: &mut dyn FnMut(&str) -> bool,
    ) -> Result<GenerateFinish, Error> {
        if prompt_tokens.is_empty() {
            return Err(Error::InvalidConfig(
                "the rendered prompt produced no tokens".to_owned(),
            ));
        }
        if prompt_tokens.len() >= self.max_position_embeddings {
            return Err(Error::InvalidConfig(format!(
                "the prompt ({} tokens) exceeds the model's maximum positions ({}); \
                 increase LoadOptions::n_ctx or shorten the conversation",
                prompt_tokens.len(),
                self.max_position_embeddings
            )));
        }
        let max_new = params
            .max_tokens
            .min((self.max_position_embeddings - prompt_tokens.len()) as u32);

        let seed = params.seed.unwrap_or_else(random_seed);
        let temperature = params.temperature;
        let sampling = if temperature < 1e-7 {
            Sampling::ArgMax
        } else if params.top_k > 0 {
            Sampling::TopKThenTopP {
                k: params.top_k as usize,
                p: params.top_p.clamp(0.0, 1.0),
                temperature,
            }
        } else {
            Sampling::TopP {
                p: params.top_p.clamp(0.0, 1.0),
                temperature,
            }
        };
        let mut sampler = LogitsProcessor::from_sampling(u64::from(seed), sampling);

        let device = self.device.clone();
        self.model.clear_kv_cache(&device);

        // Pre-fill in chunks; every chunk's last position yields logits.
        let mut logits = None;
        let mut offset = 0usize;
        let mut index = 0usize;
        while index < prompt_tokens.len() {
            let end = (index + PREFILL_CHUNK).min(prompt_tokens.len());
            logits = Some(
                self.model
                    .forward(&prompt_tokens[index..end], offset, &device)?,
            );
            offset += end - index;
            index = end;
        }
        let mut logits = logits.expect("the prompt is non-empty");

        let mut context: Vec<u32> = prompt_tokens.to_vec();
        let mut generated: Vec<u32> = Vec::new();
        let mut emitted = String::new();
        let mut finish = GenerateFinish::Length;

        loop {
            if generated.len() >= max_new as usize {
                // `finish` already holds `GenerateFinish::Length`.
                break;
            }
            let next = if params.repeat_penalty != 1.0 {
                apply_repeat_penalty(&logits, params.repeat_penalty, &context)
                    .map_err(candle_error)?
            } else {
                logits
            };
            let token = sampler.sample(&next).map_err(candle_error)?;
            if self.eos_ids.contains(&token) {
                finish = GenerateFinish::Eos;
                break;
            }
            context.push(token);
            generated.push(token);

            // Incremental decode: re-decode the generated span and emit the
            // difference; multi-byte characters split across tokens resolve
            // naturally.
            let text = self
                .tokenizer
                .decode(&generated, true)
                .map_err(|error| Error::Backend(format!("decoding failed: {error}")))?;
            if text.len() > emitted.len() && text.starts_with(&emitted) {
                let delta = text[emitted.len()..].to_owned();
                emitted = text;
                if !emit(&delta) {
                    finish = GenerateFinish::Cancelled;
                    break;
                }
            } else if text.len() < emitted.len() {
                // Non prefix-monotonic decoders: resync the baseline.
                emitted = text;
            }

            logits = self
                .model
                .forward(std::slice::from_ref(&token), offset, &device)?;
            offset += 1;
        }
        Ok(finish)
    }

    fn reset(&mut self) -> Result<(), Error> {
        let device = self.device.clone();
        self.model.clear_kv_cache(&device);
        Ok(())
    }
}

fn random_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candle_errors_map_to_backend_variant() {
        // Third-party errors never leak past the backend boundary.
        let error = candle_error(candle_core::Error::Msg("boom".to_owned()));
        assert!(
            matches!(error, Error::Backend(ref message) if message.contains("candle backend") && message.contains("boom")),
            "got: {error:?}"
        );
    }

    #[test]
    fn special_token_reads_strings_and_objects() {
        assert_eq!(special_token(&serde_json::json!("x")), Some("x".to_owned()));
        assert_eq!(
            special_token(&serde_json::json!({"content": "<|im_end|>"})),
            Some("<|im_end|>".to_owned())
        );
        assert_eq!(special_token(&serde_json::json!(42)), None);
        assert_eq!(special_token(&serde_json::Value::Null), None);
    }
}
