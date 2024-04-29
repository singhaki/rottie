#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

use std::io::Write;
use tokenizers::Tokenizer;

use candle::quantized::gguf_file;
use candle::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};

use crate::token_output_stream::TokenOutputStream;
use candle_transformers::models::quantized_llama::ModelWeights as Phi3;




pub struct phi3 {
    /// GGUF file to load, typically a .gguf file generated by the quantize command from llama.cpp
    model: Phi3,

    /// The initial prompt, use 'interactive' for entering multiple prompts in an interactive way
    /// and 'chat' for an interactive model where history of previous prompts and generated tokens
    /// is preserved.
    prompt: Option<String>,

    /// The length of the sample to generate (in tokens).
    sample_len: usize,

    /// The tokenizer config in json format.
    tokenizer: Tokenizer,

    /// The temperature used to generate samples, use 0 for greedy sampling.
    temperature: f64,

    /// Nucleus sampling probability cutoff.
    top_p: Option<f64>,

    /// Only sample among the top K samples.
    top_k: Option<usize>,

    /// The seed to use when generating random samples.
    seed: u64,

    /// Enable tracing (generates a trace-timestamp.json file).
    tracing: bool,

    /// Process prompt elements separately.
    split_prompt: bool,

    /// Run on CPU rather than GPU even if a GPU is available.
    cpu: bool,

    /// Penalty to be applied for repeating tokens, 1. means no penalty.
    repeat_penalty: f32,

    /// The context size to consider for the repeat penalty.
    repeat_last_n: usize,

    device: Device,


}

impl phi3 {
    /// Initializes the phi3 structure with default values.
    pub fn init() -> Self {
        let device = Device::new_metal(0).unwrap();

        let tokenizer = {

            let tokenizer_path = {
                let api = hf_hub::api::sync::Api::new().unwrap();
                let repo ="microsoft/Phi-3-mini-4k-instruct";
                let api = api.model(repo.to_string());
                api.get("tokenizer.json").unwrap()
            };
            Tokenizer::from_file(tokenizer_path).map_err(anyhow::Error::msg)
        };
        let model = {    
            let model_path ={
                let (repo, filename) =("microsoft/Phi-3-mini-4k-instruct-gguf",
                        "Phi-3-mini-4k-instruct-q4.gguf",
                    );
                let api = hf_hub::api::sync::Api::new().unwrap();
                let api = api.model(repo.to_string());
                api.get(filename).unwrap()
            };
            let mut file = std::fs::File::open(&model_path).unwrap();
            let start = std::time::Instant::now();
            
            let  model = {
                let model = gguf_file::Content::read(&mut file).map_err(|e| e.with_path(model_path)).unwrap();
                let mut total_size_in_bytes = 0;
                for (_, tensor) in model.tensor_infos.iter() {
                    let elem_count = tensor.shape.elem_count();
                    total_size_in_bytes +=
                        elem_count * tensor.ggml_dtype.type_size() / tensor.ggml_dtype.block_size();
                }
                println!(
                    "loaded {:?} tensors ({}) in {:.2}s",
                    model.tensor_infos.len(),
                    &format_size(total_size_in_bytes),
                    start.elapsed().as_secs_f32(),
                );
                Phi3::from_gguf(model, &mut file, &device)
            };
            println!("model built");
            model
                 
        };

        Self {
            model: model.unwrap(),
            prompt: None,
            sample_len: 1000,
            tokenizer: tokenizer.unwrap(),
            temperature: 0.8,
            top_p: None,
            top_k: None,
            seed: 299792458,
            tracing: false,
            split_prompt: false,
            cpu: false,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
            device: device,

        }
    }

    pub fn generate(&mut self,prompt: String) -> anyhow::Result<Vec<u32>> {
        // let mut phi3 = phi3::init();
        // let mut model = phi3.model()?;
        // let tokenizer = phi3.tokenizer()?;
        let mut tos = TokenOutputStream::new(self.tokenizer.clone());
        
        print!("{}", &prompt);
        let tokens = tos
            .tokenizer()
            .encode(prompt, true)
            .map_err(anyhow::Error::msg)?;
        let tokens = tokens.get_ids();
        let to_sample = self.sample_len.saturating_sub(1);
        let mut all_tokens = vec![];
        let mut logits_processor = {
            let temperature = self.temperature;
            let sampling = if temperature <= 0. {
                Sampling::ArgMax
            } else {
                match (self.top_k, self.top_p) {
                    (None, None) => Sampling::All { temperature },
                    (Some(k), None) => Sampling::TopK { k, temperature },
                    (None, Some(p)) => Sampling::TopP { p, temperature },
                    (Some(k), Some(p)) => Sampling::TopKThenTopP { k, p, temperature },
                }
            };
            LogitsProcessor::from_sampling(self.seed, sampling)
        };
        let start_prompt_processing = std::time::Instant::now();
        let mut next_token = if !self.split_prompt {
            let input = Tensor::new(tokens, &self.device)?.unsqueeze(0)?;
            let logits = self.model.forward(&input, 0)?;
            let logits = logits.squeeze(0)?;
            logits_processor.sample(&logits)?
        } else {
            let mut next_token = 0;
            for (pos, token) in tokens.iter().enumerate() {
                let input = Tensor::new(&[*token], &self.device)?.unsqueeze(0)?;
                let logits = self.model.forward(&input, pos)?;
                let logits = logits.squeeze(0)?;
                next_token = logits_processor.sample(&logits)?;
            }
            next_token
        };
        let prompt_dt = start_prompt_processing.elapsed();
        all_tokens.push(next_token);
        if let Some(t) = tos.next_token(next_token)? {
            print!("{t}");
            std::io::stdout().flush()?;
        }
        let eos_token = *tos
            .tokenizer()
            .get_vocab(true)
            .get("<|endoftext|>")
            .unwrap();
        let start_post_prompt = std::time::Instant::now();
        let mut sampled = 0;
        for index in 0..to_sample {
            let input = Tensor::new(&[next_token], &self.device)?.unsqueeze(0)?;
            let logits = self.model.forward(&input, tokens.len() + index)?;
            let logits = logits.squeeze(0)?;
            let logits = if self.repeat_penalty == 1. {
                logits
            } else {
                let start_at = all_tokens.len().saturating_sub(self.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    self.repeat_penalty,
                    &all_tokens[start_at..],
                )?
            };
            next_token = logits_processor.sample(&logits)?;
            all_tokens.push(next_token);
            if let Some(t) = tos.next_token(next_token)? {
                print!("{t}");
                std::io::stdout().flush()?;
            }
            sampled += 1;
            if next_token == eos_token {
                break;
            };
        }
        if let Some(rest) = tos.decode_rest().map_err(candle::Error::msg)? {
            print!("{rest}");
        }
        std::io::stdout().flush()?;
        let dt = start_post_prompt.elapsed();
        println!(
            "\n\n{:4} prompt tokens processed: {:.2} token/s",
            tokens.len(),
            tokens.len() as f64 / prompt_dt.as_secs_f64(),
        );
        println!(
            "{sampled:4} tokens generated: {:.2} token/s",
            sampled as f64 / dt.as_secs_f64(),
        );
    
        Ok(all_tokens)
    }
}

fn format_size(size_in_bytes: usize) -> String {
    if size_in_bytes < 1_000 {
        format!("{}B", size_in_bytes)
    } else if size_in_bytes < 1_000_000 {
        format!("{:.2}KB", size_in_bytes as f64 / 1e3)
    } else if size_in_bytes < 1_000_000_000 {
        format!("{:.2}MB", size_in_bytes as f64 / 1e6)
    } else {
        format!("{:.2}GB", size_in_bytes as f64 / 1e9)
    }
}



