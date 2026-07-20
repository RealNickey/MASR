//! Cross-platform smoke test for the CPU-only Malayalam ONNX model.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use thegai_app_lib::malayalam_asr::MalayalamAsr;

const TARGET_SAMPLE_RATE: u32 = 16_000;

fn main() -> Result<()> {
    let model_dir = std::env::var_os("MASR_MALAYALAM_MODEL_DIR")
        .map(PathBuf::from)
        .context("MASR_MALAYALAM_MODEL_DIR must point to the extracted Malayalam model")?;
    let fixture = std::env::var_os("MASR_MALAYALAM_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(default_fixture_path);
    let reference_path = std::env::var_os("MASR_MALAYALAM_REFERENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| fixture.with_file_name("reference.txt"));
    let expected = std::fs::read_to_string(&reference_path)
        .with_context(|| format!("Failed to read {}", reference_path.display()))?;
    let minimum_similarity = std::env::var("MASR_MALAYALAM_MIN_SIMILARITY")
        .ok()
        .map(|value| value.parse::<f64>())
        .transpose()
        .context("MASR_MALAYALAM_MIN_SIMILARITY must be a number")?
        .unwrap_or(0.55);

    let audio = read_mono_wav_16k(&fixture)?;
    let mut asr = MalayalamAsr::load(&model_dir).with_context(|| {
        format!(
            "Failed to load Malayalam model from {}",
            model_dir.display()
        )
    })?;
    let actual = asr.transcribe(&audio)?;

    if !actual.chars().any(is_malayalam) {
        anyhow::bail!("Malayalam smoke output contains no Malayalam Unicode: {actual:?}");
    }

    let similarity = strsim::normalized_levenshtein(&normalize(&expected), &normalize(&actual));
    println!("Malayalam smoke transcript: {actual}");
    println!("Malayalam smoke similarity: {similarity:.3}");
    if similarity < minimum_similarity {
        anyhow::bail!(
            "Malayalam smoke similarity {similarity:.3} is below the required {minimum_similarity:.3}"
        );
    }

    Ok(())
}

fn default_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("tests/fixtures/malayalam/kaley.wav")
}

fn read_mono_wav_16k(path: &Path) -> Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("Failed to open WAV fixture {}", path.display()))?;
    let spec = reader.spec();
    if spec.channels != 1 {
        anyhow::bail!(
            "Malayalam smoke fixture must be mono, got {} channels",
            spec.channels
        );
    }

    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 2_f32.powi(i32::from(spec.bits_per_sample) - 1);
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / scale))
                .collect::<Result<Vec<_>, _>>()?
        }
    };

    if spec.sample_rate == TARGET_SAMPLE_RATE {
        return Ok(samples);
    }
    if samples.is_empty() {
        return Ok(samples);
    }

    let output_len = (samples.len() as u64 * u64::from(TARGET_SAMPLE_RATE)
        / u64::from(spec.sample_rate)) as usize;
    Ok((0..output_len)
        .map(|index| {
            let source = index as f64 * f64::from(spec.sample_rate) / f64::from(TARGET_SAMPLE_RATE);
            let left = source.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (source - left as f64) as f32;
            samples[left] * (1.0 - fraction) + samples[right] * fraction
        })
        .collect())
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn is_malayalam(character: char) -> bool {
    ('\u{0D00}'..='\u{0D7F}').contains(&character)
}
