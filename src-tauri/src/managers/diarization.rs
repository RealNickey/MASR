#[cfg(feature = "diarization")]
use anyhow::Result;
#[cfg(feature = "diarization")]
use log::{debug, info};
#[cfg(feature = "diarization")]
use polyvoice::clusterer::NmeScClusterer;
#[cfg(feature = "diarization")]
use polyvoice::embedder::ResNet34Adapter;
#[cfg(feature = "diarization")]
use polyvoice::models::ModelRegistry;
#[cfg(feature = "diarization")]
use polyvoice::pipeline_v2::hybrid::HybridPipeline;
#[cfg(feature = "diarization")]
use polyvoice::segmentation::{PowersetConfig, PowersetSegmenter};
#[cfg(feature = "diarization")]
use polyvoice::types::{Profile, SampleRate};
#[cfg(feature = "diarization")]
use std::sync::Arc;
#[cfg(feature = "diarization")]
use tauri::async_runtime::Mutex;

#[cfg(feature = "diarization")]
pub struct DiarizationManager {
    pipeline: Arc<Mutex<Option<HybridPipeline>>>,
    models_dir: std::path::PathBuf,
}

#[cfg(feature = "diarization")]
impl DiarizationManager {
    pub fn new(models_dir: std::path::PathBuf) -> Self {
        Self {
            pipeline: Arc::new(Mutex::new(None)),
            models_dir,
        }
    }

    pub async fn init(&self) -> Result<()> {
        let mut pipeline_lock = self.pipeline.lock().await;
        if pipeline_lock.is_some() {
            return Ok(());
        }

        info!("Initializing DiarizationManager (polyvoice)");
        let registry = ModelRegistry::with_cache_dir(&self.models_dir)?;
        let models = registry.ensure_for_profile(Profile::Balanced)?;

        let segmenter = PowersetSegmenter::with_config(
            &models.segmenter_path,
            PowersetConfig {
                window_secs: 10.0,
                hop_secs: 1.0,
                sample_rate: 16000,
                aggregation: Default::default(),
            },
        )?;

        let pool_size = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let embedder = ResNet34Adapter::new(&models.embedder_path, pool_size)?;

        // Use 20 max speakers for NmeSc as a starting point.
        let clusterer = NmeScClusterer::new(20);

        let pipeline =
            HybridPipeline::new(Box::new(segmenter), Box::new(embedder), Box::new(clusterer));

        *pipeline_lock = Some(pipeline);
        info!("DiarizationManager initialized successfully");

        Ok(())
    }

    pub async fn diarize(
        &self,
        audio_samples: &[f32],
        sample_rate_hz: u32,
    ) -> Result<polyvoice::types::DiarizationResult> {
        let pipeline_lock = self.pipeline.lock().await;
        let pipeline = pipeline_lock
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("DiarizationManager not initialized"))?;

        let sample_rate = SampleRate::new(sample_rate_hz)
            .ok_or_else(|| anyhow::anyhow!("Invalid sample rate: {}", sample_rate_hz))?;
        debug!(
            "Running polyvoice diarization on {} samples at {}Hz",
            audio_samples.len(),
            sample_rate_hz
        );

        // This is a blocking operation, we should wrap it in spawn_blocking if it blocks the thread
        let result = pipeline.run(audio_samples, sample_rate)?;

        Ok(result)
    }
}

#[cfg(not(feature = "diarization"))]
use anyhow::Result;

#[cfg(not(feature = "diarization"))]
pub struct DiarizationManager {
    _models_dir: std::path::PathBuf,
}

#[cfg(not(feature = "diarization"))]
impl DiarizationManager {
    pub fn new(models_dir: std::path::PathBuf) -> Self {
        Self {
            _models_dir: models_dir,
        }
    }

    pub async fn init(&self) -> Result<()> {
        Ok(())
    }

    pub async fn diarize(
        &self,
        _audio_samples: &[f32],
        _sample_rate_hz: u32,
    ) -> Result<polyvoice::types::DiarizationResult> {
        Ok(polyvoice::types::DiarizationResult { segments: vec![] })
    }
}

#[cfg(not(feature = "diarization"))]
pub mod polyvoice {
    pub mod types {
        #[derive(Clone, Debug)]
        pub struct DiarizationResult {
            pub segments: Vec<Segment>,
        }

        #[derive(Clone, Debug)]
        pub struct TimeRange {
            pub start: f64,
            pub end: f64,
        }

        #[derive(Clone, Debug)]
        pub struct Segment {
            pub speaker: Option<SpeakerId>,
            pub time: TimeRange,
        }

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct SpeakerId(pub usize);
    }
}
