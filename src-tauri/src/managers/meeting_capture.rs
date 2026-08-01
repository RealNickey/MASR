//! Durable, aligned storage for an active meeting capture.
//!
//! Platform capture backends feed equal-duration 16 kHz mono frames into this
//! session. The session owns only persistence and mixing; it deliberately does
//! not know about CPAL, WASAPI, ScreenCaptureKit, or PipeWire.

use anyhow::{bail, Context, Result};
use hound::{WavSpec, WavWriter};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use super::history::AudioTracks;

const SAMPLE_RATE: u32 = 16_000;

pub struct MeetingCaptureSession {
    recordings_dir: PathBuf,
    staging_dir: PathBuf,
    final_dir_name: String,
    microphone: Option<WavWriter<std::io::BufWriter<File>>>,
    system: Option<WavWriter<std::io::BufWriter<File>>>,
    mix: Option<WavWriter<std::io::BufWriter<File>>>,
    sample_count: usize,
}

impl MeetingCaptureSession {
    pub fn create(recordings_dir: &Path) -> Result<Self> {
        fs::create_dir_all(recordings_dir)?;
        let nonce = format!(
            "{}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id()
        );
        let final_dir_name = format!("meeting-{nonce}");
        let staging_dir = recordings_dir.join(format!(".staging-{final_dir_name}"));
        fs::create_dir(&staging_dir)
            .with_context(|| format!("create meeting staging directory {staging_dir:?}"))?;

        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let microphone = WavWriter::create(staging_dir.join("microphone.wav"), spec)?;
        let system = WavWriter::create(staging_dir.join("system.wav"), spec)?;
        let mix = WavWriter::create(staging_dir.join("mix.wav"), spec)?;
        Ok(Self {
            recordings_dir: recordings_dir.to_path_buf(),
            staging_dir,
            final_dir_name,
            microphone: Some(microphone),
            system: Some(system),
            mix: Some(mix),
            sample_count: 0,
        })
    }

    /// Append one time-aligned frame from each source. Capture adapters pad a
    /// short source with silence; this preserves elapsed time and makes all
    /// persisted tracks seek-compatible.
    pub fn append_aligned(&mut self, microphone: &[f32], system: &[f32]) -> Result<()> {
        let frame_len = microphone.len().max(system.len());
        if frame_len == 0 {
            return Ok(());
        }
        let microphone_writer = self
            .microphone
            .as_mut()
            .context("meeting capture finalized")?;
        let system_writer = self.system.as_mut().context("meeting capture finalized")?;
        let mix_writer = self.mix.as_mut().context("meeting capture finalized")?;

        for index in 0..frame_len {
            let mic = microphone.get(index).copied().unwrap_or(0.0);
            let desktop = system.get(index).copied().unwrap_or(0.0);
            microphone_writer.write_sample(to_i16(mic))?;
            system_writer.write_sample(to_i16(desktop))?;
            // Averaging avoids predictable clipping while preserving both
            // sources. The final clamp protects malformed backend samples.
            mix_writer.write_sample(to_i16((mic + desktop) * 0.5))?;
        }
        self.sample_count += frame_len;
        Ok(())
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// Finalize every WAV, validate their lengths, and atomically publish the
    /// directory. A history record must be created only after this succeeds.
    pub fn finalize(mut self) -> Result<AudioTracks> {
        for writer in [&mut self.microphone, &mut self.system, &mut self.mix] {
            writer
                .take()
                .context("meeting capture finalized twice")?
                .finalize()?;
        }
        for name in ["microphone.wav", "system.wav", "mix.wav"] {
            let path = self.staging_dir.join(name);
            let samples = hound::WavReader::open(&path)?.len() as usize;
            if samples != self.sample_count {
                bail!(
                    "meeting track {name} has {samples} samples; expected {}",
                    self.sample_count
                );
            }
            File::open(&path)?.sync_all()?;
        }
        let final_dir = self.recordings_dir.join(&self.final_dir_name);
        fs::rename(&self.staging_dir, &final_dir)?;

        Ok(AudioTracks {
            mix: format!("{}/mix.wav", self.final_dir_name),
            microphone: format!("{}/microphone.wav", self.final_dir_name),
            system: format!("{}/system.wav", self.final_dir_name),
        })
    }

    pub fn discard(self) {
        let _ = fs::remove_dir_all(self.staging_dir);
    }
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finalizes_aligned_tracks_and_clamps_mix() {
        let temp = tempdir().unwrap();
        let mut session = MeetingCaptureSession::create(temp.path()).unwrap();
        session.append_aligned(&[1.0, 0.5], &[1.0]).unwrap();
        let tracks = session.finalize().unwrap();

        let mix = hound::WavReader::open(temp.path().join(&tracks.mix)).unwrap();
        let microphone = hound::WavReader::open(temp.path().join(&tracks.microphone)).unwrap();
        let system = hound::WavReader::open(temp.path().join(&tracks.system)).unwrap();
        assert_eq!(mix.len(), 2);
        assert_eq!(microphone.len(), 2);
        assert_eq!(system.len(), 2);
        assert_eq!(mix.into_samples::<i16>().next().unwrap().unwrap(), i16::MAX);
    }
}
