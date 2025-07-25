#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

use anyhow::Error as E;
use clap::Parser;

use candle::{DType, Device, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use candle_transformers::models::siglip;

use tokenizers::Tokenizer;

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
enum Which {
    #[value(name = "v1-base-patch16-224")]
    V1BasePatch16_224,
    #[value(name = "v2-base-patch16-224")]
    V2BasePatch16_224,
    #[value(name = "v2-base-patch16-256")]
    V2BasePatch16_256,
    #[value(name = "v2-base-patch16-384")]
    V2BasePatch16_384,
    #[value(name = "v2-base-patch16-512")]
    V2BasePatch16_512,
    #[value(name = "v2-large-patch16-256")]
    V2LargePatch16_256,
    #[value(name = "v2-large-patch16-384")]
    V2LargePatch16_384,
    #[value(name = "v2-large-patch16-512")]
    V2LargePatch16_512,
}

#[derive(Parser)]
struct Args {
    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    config: Option<String>,

    #[arg(long)]
    hf_repo: Option<String>,

    #[arg(long, default_value = "v1-base-patch16-224")]
    which: Which,

    #[arg(long)]
    tokenizer: Option<String>,

    #[arg(long, use_value_delimiter = true)]
    images: Option<Vec<String>>,

    #[arg(long)]
    cpu: bool,

    #[arg(long, use_value_delimiter = true)]
    sequences: Option<Vec<String>>,

    #[arg(short, long)]
    image_size: Option<usize>,
}

fn load_image<T: AsRef<std::path::Path>>(path: T, image_size: usize) -> anyhow::Result<Tensor> {
    let img = image::ImageReader::open(path)?.decode()?;
    let (height, width) = (image_size, image_size);
    let img = img.resize_to_fill(
        width as u32,
        height as u32,
        image::imageops::FilterType::Triangle,
    );
    let img = img.to_rgb8();
    let img = img.into_raw();
    let img = Tensor::from_vec(img, (height, width, 3), &Device::Cpu)?
        .permute((2, 0, 1))?
        .to_dtype(DType::F32)?
        .affine(2. / 255., -1.)?;
    Ok(img)
}

fn load_images<T: AsRef<std::path::Path>>(
    paths: &Vec<T>,
    image_size: usize,
) -> anyhow::Result<Tensor> {
    let mut images = vec![];
    for path in paths {
        let tensor = load_image(path, image_size)?;
        images.push(tensor);
    }
    let images = Tensor::stack(&images, 0)?;
    Ok(images)
}

pub fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let hf_repo = match args.hf_repo.as_ref() {
        Some(hf_repo) => hf_repo,
        None => match args.which {
            Which::V1BasePatch16_224 => "google/siglip-base-patch16-224",
            Which::V2BasePatch16_224 => "google/siglip2-base-patch16-224",
            Which::V2BasePatch16_256 => "google/siglip2-base-patch16-256",
            Which::V2BasePatch16_384 => "google/siglip2-base-patch16-384",
            Which::V2BasePatch16_512 => "google/siglip2-base-patch16-512",
            Which::V2LargePatch16_256 => "google/siglip2-large-patch16-256",
            Which::V2LargePatch16_384 => "google/siglip2-large-patch16-384",
            Which::V2LargePatch16_512 => "google/siglip2-large-patch16-512",
        },
    };
    let model_file = match args.model {
        None => {
            let api = hf_hub::api::sync::Api::new()?;
            let api = api.model(hf_repo.to_string());
            api.get("model.safetensors")?
        }
        Some(model) => model.into(),
    };
    let config_file = match args.config {
        None => {
            let api = hf_hub::api::sync::Api::new()?;
            let api = api.model(hf_repo.to_string());
            api.get("config.json")?
        }
        Some(config) => config.into(),
    };
    let tokenizer = get_tokenizer(hf_repo, args.tokenizer)?;
    let config: siglip::Config = serde_json::from_slice(&std::fs::read(config_file)?)?;
    let device = candle_examples::device(args.cpu)?;
    let vec_imgs = match args.images {
        Some(imgs) => imgs,
        None => vec![
            "candle-examples/examples/stable-diffusion/assets/stable-diffusion-xl.jpg".to_string(),
            "candle-examples/examples/yolo-v8/assets/bike.jpg".to_string(),
        ],
    };
    let images = load_images(
        &vec_imgs,
        args.image_size.unwrap_or(config.vision_config.image_size),
    )?
    .to_device(&device)?;
    let vb =
        unsafe { VarBuilder::from_mmaped_safetensors(&[model_file.clone()], DType::F32, &device)? };
    let model = siglip::Model::new(&config, vb)?;
    let (input_ids, vec_seq) = tokenize_sequences(&config, args.sequences, &tokenizer, &device)?;
    let (_logits_per_text, logits_per_image) = model.forward(&images, &input_ids)?;
    let softmax_image = softmax(&logits_per_image, 1)?;
    let softmax_image_vec = softmax_image.flatten_all()?.to_vec1::<f32>()?;
    println!("softmax_image_vec: {softmax_image_vec:?}");
    let probability_vec = softmax_image_vec
        .iter()
        .map(|v| v * 100.0)
        .collect::<Vec<f32>>();
    let probability_per_image = probability_vec.len() / vec_imgs.len();
    for (i, img) in vec_imgs.iter().enumerate() {
        let start = i * probability_per_image;
        let end = start + probability_per_image;
        let prob = &probability_vec[start..end];
        println!("\n\nResults for image: {img}\n");
        for (i, p) in prob.iter().enumerate() {
            println!("Probability: {:.4}% Text: {} ", p, vec_seq[i]);
        }
    }
    Ok(())
}

pub fn get_tokenizer(hf_repo: &str, tokenizer: Option<String>) -> anyhow::Result<Tokenizer> {
    let tokenizer = match tokenizer {
        None => {
            let api = hf_hub::api::sync::Api::new()?;
            let api = api.model(hf_repo.to_string());
            api.get("tokenizer.json")?
        }
        Some(file) => file.into(),
    };

    Tokenizer::from_file(tokenizer).map_err(E::msg)
}

pub fn tokenize_sequences(
    config: &siglip::Config,
    sequences: Option<Vec<String>>,
    tokenizer: &Tokenizer,
    device: &Device,
) -> anyhow::Result<(Tensor, Vec<String>)> {
    let pad_id = config.text_config.pad_token_id;
    let vec_seq = match sequences {
        Some(seq) => seq,
        None => vec![
            "a cycling race".to_string(),
            "a photo of two cats".to_string(),
            "a robot holding a candle".to_string(),
        ],
    };
    let mut tokens = vec![];
    for seq in vec_seq.clone() {
        let encoding = tokenizer.encode(seq, true).map_err(E::msg)?;
        tokens.push(encoding.get_ids().to_vec());
    }
    let max_len = config.text_config.max_position_embeddings;
    // Pad the sequences to have the same length
    for token_vec in tokens.iter_mut() {
        let len_diff = max_len - token_vec.len();
        if len_diff > 0 {
            token_vec.extend(vec![pad_id; len_diff]);
        }
    }
    let input_ids = Tensor::new(tokens, device)?;
    Ok((input_ids, vec_seq))
}
