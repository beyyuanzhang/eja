//! Optional local adapter. Model-specific instructions and pooling live in encoder.json.
use crate::{Encoder, InputKind, Result};
use fastembed::{
    AutoDevicePolicy, InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

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

struct LocalEncoder {
    model: TextEmbedding,
    profile: Profile,
    identity: String,
}

pub(super) fn configured() -> Result<Option<Box<dyn Encoder>>> {
    let models = Path::new(env!("CARGO_MANIFEST_DIR")).join(".models");
    let dir = std::env::var_os("EJA_MEMORY_MODEL")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            ["multilingual-e5-small", "bge-small-zh-v1.5"]
                .iter()
                .map(|n| models.join(n))
                .find(|p| p.is_dir())
        });
    dir.map(|dir| load(&dir)).transpose()
}

fn load(dir: &Path) -> Result<Box<dyn Encoder>> {
    let mut digest = Sha256::new();
    let mut read = |name: &str| -> Result<Vec<u8>> {
        let data =
            fs::read(dir.join(name)).map_err(|e| format!("reading memory model {name}: {e}"))?;
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
    let hash = format!("{:x}", digest.clone().finalize());
    let (profile, identity) = match fs::read(dir.join("encoder.json")) {
        Ok(data) => {
            digest.update(&data);
            (
                serde_json::from_slice::<Profile>(&data)?,
                format!("onnx-v1:{:x}", digest.finalize()),
            )
        }
        // Preserve the exact preprocessing and index identity of the original published adapter.
        Err(err)
            if err.kind() == std::io::ErrorKind::NotFound
                && hash == "340b8eaff11caf0f7c5b483dab04a4bbb5c9f7f04610be98fb46e01fd9e68d12" =>
        {
            (
                Profile {
                    dimensions: 512,
                    pooling: Reduction::Cls,
                    passage_prefix: String::new(),
                    query_prefix: "为这个句子生成表示以用于检索相关文章：".into(),
                    max_tokens: 512,
                },
                format!("bge-zh-cls-v1:{hash}"),
            )
        }
        Err(err) => return Err(format!("reading model profile encoder.json: {err}").into()),
    };
    if profile.max_tokens == 0 {
        return Err("model profile needs a positive token limit".into());
    }
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
    )?;
    Ok(Box::new(LocalEncoder {
        model,
        profile,
        identity,
    }))
}

impl Encoder for LocalEncoder {
    fn identity(&self) -> &str {
        &self.identity
    }
    fn dimensions(&self) -> usize {
        self.profile.dimensions
    }
    fn encode(&mut self, kind: InputKind, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let prefix = match kind {
            InputKind::Query => &self.profile.query_prefix,
            InputKind::Passage => &self.profile.passage_prefix,
        };
        if prefix.is_empty() {
            return Ok(self.model.embed(texts, Some(32))?);
        }
        let inputs: Vec<_> = texts.iter().map(|s| format!("{prefix}{s}")).collect();
        Ok(self.model.embed(inputs, Some(32))?)
    }
}
