use anyhow::Result;
use hound::{WavReader, WavSpec, WavWriter};
use log::debug;
use rodio::conversions::SampleTypeConverter;
use rodio::source::Source;
use std::path::Path;

/// Read a WAV file and return normalised f32 samples.
pub fn read_wav_samples<P: AsRef<Path>>(file_path: P) -> Result<Vec<f32>> {
    let (samples, _) = read_wav_samples_with_rate(file_path)?;
    Ok(samples)
}

/// Read a WAV file and return normalised f32 samples along with the sample rate.
pub fn read_wav_samples_with_rate<P: AsRef<Path>>(file_path: P) -> Result<(Vec<f32>, u32)> {
    let file = std::fs::File::open(file_path.as_ref())?;
    let decoder = rodio::Decoder::new(std::io::BufReader::new(file))?;
    let sample_rate = decoder.sample_rate();
    let channels = decoder.channels() as usize;

    let raw_samples: Vec<f32> = SampleTypeConverter::<_, f32>::new(decoder).collect();

    // Mix down to mono if multi-channel
    let samples = if channels > 1 {
        let mut mono = Vec::with_capacity(raw_samples.len() / channels);
        for chunk in raw_samples.chunks_exact(channels) {
            let sum: f32 = chunk.iter().sum();
            mono.push(sum / channels as f32);
        }
        mono
    } else {
        raw_samples
    };

    Ok((samples, sample_rate))
}

/// Verify a WAV file by reading it back and checking the sample count.
pub fn verify_wav_file<P: AsRef<Path>>(file_path: P, expected_samples: usize) -> Result<()> {
    let reader = WavReader::open(file_path.as_ref())?;
    let actual_samples = reader.len() as usize;
    if actual_samples != expected_samples {
        anyhow::bail!(
            "WAV sample count mismatch: expected {}, got {}",
            expected_samples,
            actual_samples
        );
    }
    Ok(())
}

/// Save audio samples as a WAV file
pub fn save_wav_file<P: AsRef<Path>>(file_path: P, samples: &[f32]) -> Result<()> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = WavWriter::create(file_path.as_ref(), spec)?;

    // Convert f32 samples to i16 for WAV
    for sample in samples {
        let sample_i16 = (sample * i16::MAX as f32) as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    debug!("Saved WAV file: {:?}", file_path.as_ref());
    Ok(())
}

/// Resample a buffer from one sample rate to another using FFT resampler
pub fn resample(input: &[f32], in_sr: usize, out_sr: usize) -> Result<Vec<f32>> {
    use rubato::{FftFixedIn, Resampler};
    let chunk = 1024usize;
    let mut r = FftFixedIn::<f32>::new(in_sr, out_sr, chunk, 1, 1)?;
    let mut out = Vec::new();
    let mut src = input;
    while src.len() >= chunk {
        let res = r.process(&[&src[..chunk]], None)?;
        out.extend_from_slice(&res[0]);
        src = &src[chunk..];
    }
    if !src.is_empty() {
        let mut pad = src.to_vec();
        pad.resize(chunk, 0.0);
        let res = r.process(&[&pad], None)?;
        out.extend_from_slice(&res[0]);
    }
    Ok(out)
}
