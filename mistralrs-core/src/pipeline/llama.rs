use super::{
    get_completion_input, get_prompt_input, Conversation, Loader, ModelKind, ModelPaths, Pipeline,
    TokenSource,
};
use crate::models::{quantized_llama, Cache};
use crate::{deref_mut_refcell, deref_refcell};
use crate::{
    models::llama::{Llama as NormalModel, LlamaConfig},
    sequence::Sequence,
    utils::{tokens::get_token, varbuilder_utils::from_mmaped_safetensors},
};
use anyhow::Result;
use candle_core::{DType, Device, Tensor};
use candle_sampling::logits_processor::Logprobs;
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use mistralrs_lora::{LoraConfig, Ordering};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::{rc::Rc, sync::Mutex};
use thiserror::Error;
use tokenizers::Tokenizer;

enum Model {
    Normal(NormalModel),
}

struct LlamaConversation;

impl Conversation for LlamaConversation {
    fn get_prompt(
        &self,
        messages: Vec<HashMap<String, String>>,
        add_generation_prompt: bool,
    ) -> Result<String, String> {
        let bos_token = "<bos>".to_string();
        if messages[0]["role"] == "system" {
            return Err("System role not supported for Llama.".to_string());
        };
        let mut content = bos_token;
        for (i, message) in messages.iter().enumerate() {
            if (message["role"] == "user") != (i % 2 == 0) {
                return Err(
                    "Conversation roles must alternate user/assistant/user/assistant/..."
                        .to_string(),
                );
            }
            let role = if message["role"] == "assistant" {
                "model".to_string()
            } else {
                message["role"].to_string()
            };
            content += &format!(
                "<start_of_turn>{role}\n{}<end_of_turn>\n",
                message["content"].trim()
            )
        }
        if add_generation_prompt {
            content += "<start_of_turn>model\n"
        }
        Ok(content)
    }
}

pub struct LlamaModelPaths<P> {
    tokenizer_filename: P,
    config_filename: P,
    filenames: Vec<P>,
    xlora_adapter_filenames: Option<Vec<(String, P)>>,
    xlora_adapter_configs: Option<Vec<(String, LoraConfig)>>,
    classifier_path: Option<P>,
    classifier_config: Option<P>,
    xlora_ordering: Option<Ordering>,
}

impl ModelPaths for LlamaModelPaths<PathBuf> {
    fn get_config_filename(&self) -> &PathBuf {
        &self.config_filename
    }
    fn get_tokenizer_filename(&self) -> &PathBuf {
        &self.tokenizer_filename
    }
    fn get_weight_filenames(&self) -> &[PathBuf] {
        &self.filenames
    }
    fn get_adapter_filenames(&self) -> &Option<Vec<(String, PathBuf)>> {
        &self.xlora_adapter_filenames
    }
    fn get_adapter_configs(&self) -> &Option<Vec<(String, LoraConfig)>> {
        &self.xlora_adapter_configs
    }
    fn get_classifier_config(&self) -> &Option<PathBuf> {
        &self.classifier_config
    }
    fn get_classifier_path(&self) -> &Option<PathBuf> {
        &self.classifier_path
    }
    fn get_ordering(&self) -> &Option<Ordering> {
        &self.xlora_ordering
    }
}

pub struct LlamaPipeline {
    model: Model,
    tokenizer: Tokenizer,
    config: LlamaSpecificConfig,
    no_xlora_kv_cache: bool,
}

pub struct LlamaLoader {
    model_id: String,
    config: LlamaSpecificConfig,
    quantized_model_id: Option<String>,
    quantized_filename: Option<String>,
    xlora_model_id: Option<String>,
    kind: ModelKind,
    xlora_order: Option<Ordering>,
    no_xlora_kv_cache: bool,
}

#[derive(Clone, Copy)]
pub struct LlamaSpecificConfig {
    pub repeat_last_n: usize,
    pub use_flash_attn: bool,
}

#[derive(Error, Debug)]
enum TokenizerError {
    #[error("`{0}`")]
    Error(String),
}

impl LlamaLoader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        config: LlamaSpecificConfig,
        quantized_model_id: Option<String>,
        quantized_filename: Option<String>,
        xlora_model_id: Option<String>,
        kind: ModelKind,
        xlora_order: Option<Ordering>,
        no_xlora_kv_cache: bool,
    ) -> Self {
        Self {
            model_id,
            config,
            quantized_model_id,
            quantized_filename,
            xlora_model_id,
            kind,
            xlora_order,
            no_xlora_kv_cache,
        }
    }
}

impl Loader for LlamaLoader {
    fn download_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
    ) -> Result<Box<dyn ModelPaths>> {
        let api = ApiBuilder::new()
            .with_progress(true)
            .with_token(Some(get_token(&token_source)?))
            .build()?;
        let revision = revision.unwrap_or("main".to_string());
        let api = api.repo(Repo::with_revision(
            self.model_id.clone(),
            RepoType::Model,
            revision.clone(),
        ));

        let tokenizer_filename = api.get("tokenizer.json")?;

        let config_filename = api.get("config.json")?;

        let filenames = match &self.quantized_filename {
            Some(name) => {
                let qapi = ApiBuilder::new()
                    .with_progress(true)
                    .with_token(Some(get_token(&token_source)?))
                    .build()?;
                let qapi = qapi.repo(Repo::with_revision(
                    self.quantized_model_id.as_ref().unwrap().clone(),
                    RepoType::Model,
                    revision.clone(),
                ));
                vec![qapi.get(name).unwrap()]
            }
            None => {
                let mut filenames = vec![];
                for rfilename in api
                    .info()?
                    .siblings
                    .iter()
                    .map(|x| x.rfilename.clone())
                    .filter(|x| x.ends_with(".safetensors"))
                {
                    let filename = api.get(&rfilename)?;
                    filenames.push(filename);
                }
                filenames
            }
        };

        let (adapters_configs, adapters_safetensors, classifier_path, classifier_config, ordering) =
            if let Some(ref xlora_id) = self.xlora_model_id {
                let api = ApiBuilder::new()
                    .with_progress(true)
                    .with_token(Some(get_token(&token_source)?))
                    .build()?;
                let api = api.repo(Repo::with_revision(
                    xlora_id.clone(),
                    RepoType::Model,
                    revision,
                ));
                let xlora_classifier = &api
                    .info()?
                    .siblings
                    .iter()
                    .map(|x| x.rfilename.clone())
                    .filter(|x| x.contains("xlora_classifier.safetensors"))
                    .collect::<Vec<_>>()[0];
                let xlora_config = &api
                    .info()?
                    .siblings
                    .iter()
                    .map(|x| x.rfilename.clone())
                    .filter(|x| x.contains("xlora_config.json"))
                    .collect::<Vec<_>>()[0];
                let classifier_path = api.get(xlora_classifier)?;
                let config_path = api.get(xlora_config)?;

                let adapter_files = api
                    .info()?
                    .siblings
                    .iter()
                    .map(|x| x.rfilename.clone())
                    .filter(|x| x.contains("/adapter_"))
                    .map(|x| {
                        let mut split = x.split('/');
                        let pos = split.clone().count() - 2;
                        let name = split.nth(pos).unwrap().to_string();
                        (x, name)
                    })
                    .collect::<Vec<_>>();
                let mut adapters_paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
                for (file, name) in adapter_files {
                    if let Some(paths) = adapters_paths.get_mut(&name) {
                        paths.push(api.get(&file)?);
                    } else {
                        adapters_paths.insert(name, vec![api.get(&file)?]);
                    }
                }
                let mut adapters_configs = Vec::new();
                let mut adapters_safetensors = Vec::new();
                for name in &self.xlora_order.as_ref().unwrap().adapters {
                    let paths = adapters_paths.get(name).unwrap();
                    for path in paths {
                        if path.extension().unwrap() == "safetensors" {
                            adapters_safetensors.push((name.clone(), path.to_owned()));
                        } else {
                            let conf = fs::read_to_string(path)?;
                            let lora_config: LoraConfig = serde_json::from_str(&conf)?;
                            adapters_configs.push((name.clone(), lora_config));
                        }
                    }
                }
                (
                    Some(adapters_configs),
                    Some(adapters_safetensors),
                    Some(classifier_path),
                    Some(config_path),
                    self.xlora_order.clone(),
                )
            } else {
                (None, None, None, None, None)
            };

        Ok(Box::new(LlamaModelPaths {
            tokenizer_filename,
            config_filename,
            filenames,
            xlora_adapter_configs: adapters_configs,
            xlora_adapter_filenames: adapters_safetensors,
            classifier_path,
            classifier_config,
            xlora_ordering: ordering,
        }))
    }

    fn _setup_model(
        &self,
        paths: &dyn ModelPaths,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<(
        Box<Mutex<dyn Pipeline>>,
        Arc<dyn Conversation + Send + Sync>,
    )> {
        let basic_config: LlamaConfig =
            serde_json::from_slice(&std::fs::read(paths.get_config_filename())?)?;
        let default_dtype = if device.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };

        println!("Loading model on {device:?}...");
        let model = match self.kind {
            ModelKind::QuantizedGGUF => unreachable!(),
            ModelKind::QuantizedGGML => unreachable!(),
            ModelKind::Normal => {
                let vb = from_mmaped_safetensors(
                    paths.get_weight_filenames().to_vec(),
                    Vec::new(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = NormalModel::load(
                    vb,
                    &basic_config.into_config(self.config.use_flash_attn),
                    default_dtype,
                    device,
                )?;
                Model::Normal(model)
            }
            ModelKind::XLoraNormal => unreachable!(),
        };
        println!("Model loaded.");

        let tokenizer = Tokenizer::from_file(paths.get_tokenizer_filename())
            .map_err(|e| TokenizerError::Error(e.to_string()))?;

        Ok((
            Box::new(Mutex::new(LlamaPipeline {
                model,
                tokenizer,
                config: self.config,
                no_xlora_kv_cache: self.no_xlora_kv_cache,
            })),
            Arc::new(LlamaConversation),
        ))
    }
}

impl Pipeline for LlamaPipeline {
    fn forward(&mut self, input_toks: Box<[Rc<RefCell<Sequence>>]>, is_prompt: bool) -> Tensor {
        let (input_ids, _input_ids_full, seqlen_offsets, _seqlen_offsets_full) =
            if self.is_xlora() && !is_prompt {
                let (input_ids_full, seqlen_offsets_full) =
                    get_prompt_input(&input_toks, self.device());
                let (input_ids, seqlen_offsets) = get_completion_input(&input_toks, self.device());
                (
                    input_ids,
                    Some(input_ids_full),
                    seqlen_offsets,
                    Some(seqlen_offsets_full),
                )
            } else if self.is_xlora() && is_prompt {
                let (input_ids_full, seqlen_offsets) = get_prompt_input(&input_toks, self.device());
                (
                    input_ids_full.clone(),
                    Some(input_ids_full),
                    seqlen_offsets.clone(),
                    Some(seqlen_offsets),
                )
            } else if is_prompt {
                let (input_ids, seqlen_offsets) = get_prompt_input(&input_toks, self.device());
                (input_ids, None, seqlen_offsets, None)
            } else {
                let (input_ids, seqlen_offsets) = get_completion_input(&input_toks, self.device());
                (input_ids, None, seqlen_offsets, None)
            };
        let result = match self.model {
            Model::Normal(ref mut model) => model.forward(&input_ids, &seqlen_offsets),
        };
        match result {
            Ok(v) => v,
            Err(e) => {
                panic!("Model failed with error `{e}`. Please raise an issue.");
            }
        }
    }
    fn tokenize_prompt(&self, prompt: &str) -> Result<Vec<u32>> {
        let encoding = self
            .tokenizer
            .encode(prompt, false)
            .map_err(|e| anyhow::Error::msg(e.to_string()))?;
        Ok(encoding.get_ids().to_vec())
    }
    fn device(&self) -> &Device {
        match self.model {
            Model::Normal(ref model) => &model.device,
        }
    }
    fn num_hidden_layers(&self) -> usize {
        self.cache().lock().len()
    }
    fn cache(&self) -> &Cache {
        match self.model {
            Model::Normal(ref model) => &model.kv_cache,
        }
    }
    fn sample(&mut self, logits: Tensor, seq: Rc<RefCell<Sequence>>) -> Result<Logprobs> {
        let logits = logits
            .squeeze(0)
            .unwrap()
            .squeeze(0)
            .unwrap()
            .to_dtype(DType::F32)
            .unwrap();
        let start_at = deref_refcell!(seq)
            .get_toks()
            .len()
            .saturating_sub(self.config.repeat_last_n);
        let ctxt = deref_refcell!(seq).get_toks()[start_at..].to_vec();

        Ok(deref_mut_refcell!(seq)
            .logits_processor()
            .sample(&logits, Some(&ctxt))?)
    }
    fn tokenizer(&self) -> Tokenizer {
        self.tokenizer.clone()
    }
    fn eos_tok(&self) -> u32 {
        self.tokenizer
            .get_vocab(true)
            .get("<eos>")
            .copied()
            .expect("Unable to extract `<eos>` EOS token.")
    }
    fn name(&self) -> &'static str {
        "llama"
    }
    fn get_max_seq_len(&self) -> usize {
        match &self.model {
            Model::Normal(_) => quantized_llama::MAX_SEQ_LEN as usize,
        }
    }
    fn is_xlora(&self) -> bool {
        match &self.model {
            Model::Normal(_) => false,
        }
    }
    fn has_no_xlora_kv_cache(&self) -> bool {
        self.no_xlora_kv_cache
    }
}