//! Optional local ONNX adapter. Model-specific instructions and pooling live in encoder.json.
use crate::{Encoder, Error, InputKind, Result};
use fastembed::{
    AutoDevicePolicy, InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reduction {
    Mean,
    Cls,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    dimensions: usize,
    pooling: Reduction,
    #[serde(default)]
    passage_prefix: String,
    #[serde(default)]
    query_prefix: String,
    max_tokens: usize,
}

#[derive(Clone)]
pub struct OnnxEncoder {
    model: Arc<Mutex<TextEmbedding>>,
    profile: Arc<Profile>,
    identity: Arc<str>,
}

impl OnnxEncoder {
    /// Load a local ONNX embedding model from a directory containing `model_quantized.onnx`,
    /// tokenizer files, and `encoder.json`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let mut digest = Sha256::new();
        let mut read = |name: &str| -> Result<Vec<u8>> {
            let data = fs::read(dir.join(name))
                .map_err(|err| Error::Invalid(format!("reading memory model {name}: {err}")))?;
            digest.update((data.len() as u64).to_le_bytes());
            digest.update(&data);
            Ok(data)
        };
        let weights = read("model_quantized.onnx")?;
        let files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let profile_data = read("encoder.json")?;
        let profile: Profile = serde_json::from_slice(&profile_data)?;
        if profile.max_tokens == 0 {
            return Err(Error::Invalid(
                "model profile needs a positive token limit".into(),
            ));
        }
        let identity = format!("onnx-v1:{:x}", digest.finalize());
        let pooling = match profile.pooling {
            Reduction::Mean => Pooling::Mean,
            Reduction::Cls => Pooling::Cls,
        };
        let model = TextEmbedding::try_new_from_user_defined(
            UserDefinedEmbeddingModel::new(weights, files).with_pooling(pooling),
            InitOptionsUserDefined::new()
                .with_auto_device(AutoDevicePolicy::PreferCPU)
                .with_intra_threads(4)
                .with_max_length(profile.max_tokens),
        )
        .map_err(|err| Error::Backend(err.to_string()))?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
            profile: Arc::new(profile),
            identity: Arc::from(identity),
        })
    }
}

impl Encoder for OnnxEncoder {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn dimensions(&self) -> usize {
        self.profile.dimensions
    }
    fn encode(
        &mut self,
        kind: InputKind,
        texts: &[&str],
    ) -> std::result::Result<Vec<Vec<f32>>, Box<dyn std::error::Error + Send + Sync>> {
        let mut model = self
            .model
            .lock()
            .map_err(|_| std::io::Error::other("onnx encoder mutex poisoned"))?;
        let prefix = match kind {
            InputKind::Query => &self.profile.query_prefix,
            InputKind::Passage => &self.profile.passage_prefix,
        };
        if prefix.is_empty() {
            return Ok(model.embed(texts, Some(32))?);
        }
        let inputs: Vec<_> = texts.iter().map(|text| format!("{prefix}{text}")).collect();
        Ok(model.embed(inputs, Some(32))?)
    }
}
