use std::fs::File;
use std::process::exit;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, Stream, StreamConfig};
use biquad::{ Coefficients, DirectForm1, ToHertz};
use symphonia::core::audio::{AudioBufferRef, SampleBuffer};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions};
use symphonia::core::probe::Hint;

use crate::{
    effects::Effects,
    enums::{DECODE_BACKPRESSURE_SLEEP_MS, DEFAULT_VISUALIZER_BAR_COUNT, MAX_RATE, MIN_RATE},
    processor::VisualizerProcessor,
    source::AudioSource,
    http_stream::HttpStream,
    player_state::{PlayerState, ResampleState, SharedPlayback},
    info, error, warn,
};

/// The main audio engine struct that manages audio playback, decoding, and processing.
pub struct AudioEngine {
    /// Shared state between the audio output callback and the main thread. 
    /// This includes the playback queue, visualizer data, and playback parameters.
    shared: Arc<Mutex<SharedPlayback>>,

    /// Flag indicating whether the audio stream has been started.
    stream_started: bool,

    /// The currently loaded audio source, which can be a file or a URL.
    source: Option<AudioSource>,

    /// Thread handle for the decode thread, 
    /// which is responsible for decoding the audio source and feeding it into the playback queue.
    decode_thread: Option<JoinHandle<()>>,

    /// Flag to signal the decode thread to stop. This is used when changing sources or stopping playback.
    decode_stop: Arc<AtomicBool>,

    /// Number of output channels, determined from the audio device configuration. 
    /// This is used for resampling and buffering calculations.
    out_channels: usize,

    /// Output sample rate, determined from the audio device configuration.
    out_sample_rate: u32,

    /// Processor for computing visualizer data from the audio samples. 
    /// This is used to generate the spectrum data for the visualizer.
    visualizer_processor: VisualizerProcessor,

    /// Estimated duration of the loaded audio source in milliseconds. 
    /// This is calculated when a source is loaded and can be used for seeking and displaying duration.
    source_duration_millis: i32,

    /// The starting position for decoding in milliseconds. 
    /// This is updated when seeking to indicate where the decode thread should start decoding from.
    decode_start_millis: i32,

    /// The CPAL audio stream. This is kept as a field to ensure it stays alive for the lifetime of the AudioEngine.
    audio_stream: Option<Stream>,
}

impl AudioEngine {
    /// Create a new instance of the audio engine. 
    /// This initializes the shared state and sets up the audio configuration.
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No default output device available".to_string())?;

        let config = device
            .default_output_config()
            .map_err(|e| format!("Failed to read default output config: {e}"))?;

        let out_channels = config.channels() as usize;
        let out_sample_rate = config.sample_rate();

        info!("Engine Initialized");
        Ok(Self {
            shared: Arc::new(Mutex::new(SharedPlayback::new(
                out_channels,
                out_sample_rate,
            ))),
            stream_started: false,
            source: None,
            decode_thread: None,
            decode_stop: Arc::new(AtomicBool::new(false)),
            out_channels,
            out_sample_rate,
            visualizer_processor: VisualizerProcessor::new(DEFAULT_VISUALIZER_BAR_COUNT),
            source_duration_millis: -1,
            decode_start_millis: 0,
            audio_stream: None,
        })
    }

    /// Initialize the audio output stream. 
    /// This is called lazily when playback starts to avoid issues on platforms 
    /// where the audio device may not be ready at app startup.
    /// 
    /// Returns an error if the stream fails to initialize, 
    /// which can happen if the audio device is unavailable.
    pub fn ensure_stream(&mut self) -> Result<(), String> {
        if self.stream_started {
            return Ok(());
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No default output device available".to_string())?;

        let output_config = device
            .default_output_config()
            .map_err(|e| format!("Failed to get default output config: {e}"))?;

        let sample_format = output_config.sample_format();
        let stream_config = StreamConfig {
            channels: output_config.channels(),
            sample_rate: (self.out_sample_rate),
            buffer_size: BufferSize::Default,
        };

        let shared = Arc::clone(&self.shared);
        let channels = self.out_channels;

        let err_fn = |err| {
            error!("CPAL stream error: {err}");
        };
 
        let stream = match sample_format {
            SampleFormat::F32 => device
                .build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _| write_output_f32(data, channels, &shared),
                    err_fn,
                    None,
                )
                .map_err(|e| format!("Failed to build f32 output stream: {e}"))?,
            SampleFormat::I16 => {
                let shared_i16 = Arc::clone(&self.shared);
                device
                    .build_output_stream(
                        &stream_config,
                        move |data: &mut [i16], _| write_output_i16(data, channels, &shared_i16),
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Failed to build i16 output stream: {e}"))?
            }
            SampleFormat::U16 => {
                let shared_u16 = Arc::clone(&self.shared);
                device
                    .build_output_stream(
                        &stream_config,
                        move |data: &mut [u16], _| write_output_u16(data, channels, &shared_u16),
                        err_fn,
                        None,
                    )
                    .map_err(|e| format!("Failed to build u16 output stream: {e}"))?
            }
            _ => return Err("Unsupported sample format".to_string()),
        };

        stream
            .play()
            .map_err(|e| format!("Failed to start output stream: {e}"))?;

        // Keep the CPAL stream alive for the AudioEngine's lifetime.
        self.audio_stream = Some(stream);
        self.stream_started = true;
        Ok(())
    }

    /// Set the audio source to be played. This can be a file path or a URL.
     /// The source is loaded and decoded in a separate thread to avoid blocking the main thread.
     /// If a source is already playing, it will be stopped and replaced with the new source.
     /// 
     /// Returns an error if the source fails to load, which can happen if the file doesn't exist
     /// or if there's a network error when loading from a URL.
    pub fn set_source(&mut self, source: AudioSource) {
        self.source_duration_millis = estimate_duration_millis(&source, self.out_channels as u32);
        self.decode_start_millis = 0;
        self.source = Some(source);
        self.stop_decode_thread();
        if let Ok(mut shared) = self.shared.lock() {
            shared.clear_audio_state();
            shared.stream_finished = false;
        }
        self.visualizer_processor.reset();
    }
 
    pub fn set_playing(&mut self, playing: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            if playing {
                shared.stream_finished = false;
            }
            shared.playing = playing;
        }
    }

    /// Stop the decode thread if it's running. This is called when changing sources 
    /// or stopping playback.
    /// 
    /// It signals the thread to stop and waits for it to finish before returning.
    pub fn stop(&mut self) {
        self.set_playing(false);
        self.stop_decode_thread();
        if let Ok(mut shared) = self.shared.lock() {
            shared.clear_audio_state();
        }
        self.visualizer_processor.reset();
    }

    pub fn set_volume(&mut self, volume: f32) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.volume = volume.clamp(0.0, 4.0);
        }
    }

    pub fn set_max_queue_seconds(&mut self, seconds: usize) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.set_max_queue_seconds(self.out_channels, seconds);
        }
    }

    pub fn seek(&mut self, millis: i32) {
        let mut target = millis.max(0);
        if self.source_duration_millis > 0 {
            target = target.min(self.source_duration_millis);
        }
 
        let should_resume = self.is_playing() == 1;
        // Reset state and stop decode thread before updating the decode start time to avoid race conditions 
        // where the decode thread is still running and checks the old decode_start_millis after we've updated it.
        self.stop_decode_thread();
        self.decode_start_millis = target;

        let target_samples = ((target as u64)
            .saturating_mul(self.out_sample_rate as u64)
            .saturating_mul(self.out_channels as u64)
            / 1000) as u64;

        if let Ok(mut shared) = self.shared.lock() {
            shared.queue.clear();
            shared.visualizer_ring.clear();
            shared.emitted_samples = 0;
            shared.source_offset_samples = target_samples;
            shared.stream_finished = false;
            shared.playing = false;
        }
        // Reset the visualizer state to avoid showing a burst of activity 
        // when seeking to a new position with different audio content.
        self.visualizer_processor.reset();

        if should_resume {
            let _ = self.start_decode_thread_if_needed();
            self.set_playing(true);
        }
    }

    /// Get the current playback position in milliseconds. 
    /// This is calculated based on the number of samples emitted
    pub fn position_millis(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => {
                if shared.sample_rate == 0 || self.out_channels == 0 {
                    return 0;
                }

                let total_output_samples =
                    shared.source_offset_samples.saturating_add(shared.emitted_samples);

                ((shared.playback_rate as f64) * (total_output_samples as f64
                    / (shared.sample_rate as f64 * self.out_channels as f64)
                    * 1000.0))  as i32
            }
            Err(_) => -1,
        }
    }

    /// Get the total duration of the loaded audio source in milliseconds.
    pub fn duration_millis(&self) -> i32 {
        self.source_duration_millis
    }

    /// Get maximum queue duration in seconds. 
    /// This is the maximum amount of audio that will be buffered in memory.
    pub fn max_queue_seconds(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => shared.max_queue_seconds as i32,
            Err(_) => -1,
        }
    }

    /// Return size of the buffered audio data in samples. 
    /// This is the amount of audio data currently buffered and ready for playback.
    /// This can be used to monitor buffering progress or to implement custom buffering strategies.
    pub fn buffered_samples(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => shared.queue.len() as i32,
            Err(_) => -1,
        }
    }

    /// Return size of the buffered audio data in milliseconds.
    /// Returns -1 if the sample rate or channel count is invalid, 
    /// which would prevent accurate conversion to milliseconds.
    pub fn buffered_millis(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => {
                if shared.sample_rate == 0 {
                    return 0;
                }
                ((shared.queue.len() as f64 / self.out_channels.max(1) as f64)
                    / shared.sample_rate as f64
                    * 1000.0) as i32
            }
            Err(_) => -1,
        }
    }

    /// Return the number of samples available for the visualizer.  
    pub fn visualizer_available_samples(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => shared.visualizer_ring.len() as i32,
            Err(_) => -1,
        }
    }

    pub fn visualizer_sample_rate(&self) -> i32 {
        self.out_sample_rate as i32
    }

    pub fn visualizer_channels(&self) -> i32 {
        self.out_channels as i32
    }

    /// Copy the lasest visualizer samples into the provided buffer.
    pub fn copy_visualizer_samples(&self, out: &mut [f32]) -> i32 {
        match self.shared.lock() {
            Ok(shared) => shared.copy_latest_visualizer_samples(out) as i32,
            Err(_) => -1,
        }
    }

    /// Clear all audio filters. 
    /// This will reset the audio processing chain to a clean state with no effects applied.
    pub fn clear_filters(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            for effect in &mut shared.effects {
                *effect = Effects::new();
            }
        }
    }

    /// Set the playback rate. This will speed up or slow down the audio without changing the pitch.
    /// The rate is a multiplier where 1.0 is normal speed, 0.5 is half speed, and 2.0 is double speed.
    pub fn set_rate(&mut self, rate: f32) {
        let rate = rate.clamp(MIN_RATE, MAX_RATE);

        if let Ok(mut shared) = self.shared.lock() {
            shared.playback_rate = rate;
        }
    }

    /// Get the current playback rate. This is the multiplier applied to the audio speed, where 1.0 is normal speed.
    /// Returns -1 if the shared state cannot be accessed, which would prevent retrieving the playback rate.
    pub fn rate(&self) -> f32 {
        match self.shared.lock() {
            Ok(shared) => shared.playback_rate,
            Err(_) => 1.0,
        }
    }
    
    /// Set a peaking EQ filter with the specified cutoff frequency, gain, and Q factor.
    pub fn set_peak_filter(&mut self, cutoff_hz: f32, gain_db: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate;
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::PeakingEQ(gain_db),
                        sample_rate.hz(),
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.peak = Some(DirectForm1::new(coeffs));
                } else {
                    effect.peak = None;
                }
            }
        }
    }

    /// Set a comb filter with the specified delay and feedback. 
    /// A comb filter creates a series of notches in the frequency response, which can produce effects like flanging or reverb.
    pub fn set_comb_filter(&mut self, delay_ms: f32, feedback: f32) {
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate;
            for effect in &mut shared.effects {
                if delay_ms > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::AllPass,
                        sample_rate.hz(),
                        (1000.0 / delay_ms).hz(),
                        feedback,
                    )
                    .unwrap();
                    effect.comb = Some(DirectForm1::new(coeffs));
                } else {
                    effect.comb = None;
                }
            }
        }
    }

    /// Check if the provided filter parameters are valid. 
    /// This is used by the various filter-setting methods to validate the cutoff frequency and Q factor before applying the filter.
    pub fn filter_check(&self, cutoff_hz: f32, q: f32) -> i8 {
        let fs = self.shared.lock().unwrap().sample_rate as f32;
        if cutoff_hz > 0.0 && cutoff_hz < fs / 2.0 && q > 0.0 {
            info!("Filter params: cutoff={} Hz, Q={}", cutoff_hz, q);
            0
        } else if q <= 0.0 {
            error!("Invalid filter Q: {}. Must be > 0.", q);
            -1
        } else if cutoff_hz <= 0.0 {
            warn!("Disabling filter because cutoff_hz is {}.", cutoff_hz);
            0
        } else {
            error!(
                "Invalid cutoff frequency: {} Hz. Must be > 0 and < Nyquist ({} Hz).",
                cutoff_hz,
                fs / 2.0
            );
            -1
        }
    }

    /// Set the low-shelf filter parameters. A low-shelf filter boosts or cuts frequencies below the cutoff frequency.
    pub fn set_low_shelf_filter(&mut self, cutoff_hz: f32, gain_db: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate.hz();
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::LowShelf(gain_db),
                        sample_rate,
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.low_shelf = Some(DirectForm1::new(coeffs));
                } else {
                    effect.low_shelf = None;
                }
            }
        }
    }

    /// Set the high-shelf filter parameters. A high-shelf filter boosts or cuts frequencies above the cutoff frequency.
    pub fn set_high_shelf_filter(&mut self, cutoff_hz: f32, gain_db: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            exit(1);
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate.hz();
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::HighShelf(gain_db),
                        sample_rate,
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.high_shelf = Some(DirectForm1::new(coeffs));
                } else {
                    effect.high_shelf = None;
                }
            }
        }
    }

    /// Set the band-pass filter parameters. A band-pass filter allows frequencies around the center frequency to pass through while attenuating frequencies outside that range.
    pub fn set_band_pass_filter(&mut self, center_hz: f32, q: f32) {
        if self.filter_check(center_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate.hz();
            for effect in &mut shared.effects {
                if center_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::BandPass,
                        sample_rate,
                        center_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.band_pass = Some(DirectForm1::new(coeffs));
                } else {
                    effect.band_pass = None;
                }
            }
        }
    }

    /// set the low-pass filter parameters. 
    /// A low-pass filter attenuates frequencies above the cutoff frequency, 
    /// allowing lower frequencies to pass through.
    /// 
    /// If `cutoff_hz` is set to 0 or a negative value, 
    /// the low-pass filter will be disabled.
    /// The `q` parameter controls the resonance of the filter. 
    /// Higher Q values result in a sharper cutoff around the cutoff frequency.
    /// 
    /// Returns an error if the parameters are invalid, 
    /// such as a cutoff frequency above Nyquist or a non-positive Q value.
    pub fn set_lowpass_filter(&mut self, cutoff_hz: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate;
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::LowPass,
                        sample_rate.hz(),
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.low_pass = Some(DirectForm1::new(coeffs));
                } else {
                    effect.low_pass = None;
                }
            }
        }
    }

    /// Set the high-pass filter parameters. 
    /// A high-pass filter attenuates frequencies below the cutoff frequency,
    /// allowing higher frequencies to pass through.
    /// 
    /// If `cutoff_hz` is set to 0 or a negative value,
    /// the high-pass filter will be disabled.
    /// The `q` parameter controls the resonance of the filter.
    /// Higher Q values result in a sharper cutoff around the cutoff frequency.
    /// Returns an error if the parameters are invalid,
    /// such as a cutoff frequency above Nyquist or a non-positive Q value.
    pub fn set_high_pass_filter(&mut self, cutoff_hz: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate.hz();
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::HighPass,
                        sample_rate,
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.high_pass = Some(DirectForm1::new(coeffs));
                } else {
                    effect.high_pass = None;
                }
            }
        }
    }

    /// Set the notch filter parameters. 
    /// A notch filter attenuates frequencies around the cutoff frequency, creating a "notch" in the frequency response.
    pub fn set_notch_filter(&mut self, cutoff_hz: f32, q: f32) {
        if self.filter_check(cutoff_hz, q) != 0 {
            return;
        }
        if let Ok(mut shared) = self.shared.lock() {
            let sample_rate = shared.sample_rate.hz();
            for effect in &mut shared.effects {
                if cutoff_hz > 0.0 {
                    let coeffs = Coefficients::<f32>::from_params(
                        biquad::Type::Notch,
                        sample_rate,
                        cutoff_hz.hz(),
                        q,
                    )
                    .unwrap();
                    effect.notch = Some(DirectForm1::new(coeffs));
                } else {
                    effect.notch = None;
                }
            }
        }
    }

    /// Copy the latest visualizer spectrum data into the provided buffer.
    pub fn copy_visualizer_spectrum(&mut self, out: &mut [f32]) -> i32 {
        let (snapshot, playing) = match self.shared.lock() {
            Ok(shared) => (shared.visualizer_ring.iter().copied().collect::<Vec<f32>>(), shared.playing),
            Err(_) => return -1,
        };

        self.visualizer_processor
            .compute(&snapshot, self.out_channels, self.out_sample_rate, out, playing)
    }

    /// Check if the player is currently playing. 
    /// Returns 1 if playing, 0 if paused or stopped, and -1 if the state cannot be determined.
    /// This can be used to update UI elements or to implement custom playback logic based on the current state.
    pub fn is_playing(&self) -> i32 {
        match self.shared.lock() {
            Ok(shared) => i32::from(shared.playing),
            Err(_) => -1,
        }
    }

    /// Get the current player state as an enum. 
    /// This provides more detailed information about the playback state, 
    /// including whether it's idle, playing, paused, or stopped.
    pub fn get_state(&self) -> PlayerState {
        if self.source.is_none() {
            return PlayerState::Idle;
        }
        match self.shared.lock() {
            Ok(shared) => {
                if shared.playing {
                    PlayerState::Playing
                } else if shared.stream_finished {
                    PlayerState::Stopped
                } else {
                    PlayerState::Paused
                }
            }
            Err(_) => PlayerState::Idle,
        }
    }

    pub fn is_source_loaded(&self) -> i32 {
        i32::from(self.source.is_some())
    }
    
    /// Start the decode thread if it's not already running. 
    /// This thread is responsible for decoding the audio source and feeding it into the playback queue.
    pub fn start_decode_thread_if_needed(&mut self) -> Result<(), String> {
        if self.decode_thread.is_some() {
            return Ok(());
        }

        let source = self
            .source
            .clone()
            .ok_or_else(|| "No source loaded. Call set_source first.".to_string())?;

        self.decode_stop.store(false, Ordering::SeqCst);
        let stop_flag = Arc::clone(&self.decode_stop);
        let shared = Arc::clone(&self.shared);
        let out_channels = self.out_channels;
        let out_sample_rate = self.out_sample_rate;
        let start_millis = self.decode_start_millis;

        if let Ok(mut state) = self.shared.lock() {
            state.stream_finished = false;
        }

        let handle = thread::spawn(move || {
            if let Err(err) = decode_and_feed(
                source,
                stop_flag,
                shared,
                out_channels,
                out_sample_rate,
                start_millis,
            ) {
                error!("Decode thread ended with error: {err}");
            }
        });

        self.decode_thread = Some(handle);
        Ok(())
    }

    /// Extracts metadata from the media file, including tags and codec information.
    pub fn get_metadata(&self, path: &str) -> Result<String, String> {
        let single_source: BoxedMediaSource = if path.starts_with("http://") || path.starts_with("https://") {
            let http_stream = HttpStream::new(path)?;
            Box::new(http_stream)
        } else {
            let file = File::open(&path)
                .map_err(|e| format!("Failed to open file source '{path}': {e}"))?;
            Box::new(file)
        };

        let mss = MediaSourceStream::new(single_source, MediaSourceStreamOptions::default());
        let hint = Hint::new();
        let mut probed = match symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        ) {
            Ok(p) => p,
            Err(_) => return Err("Failed to probe format".into()),
        };

        let mut metadata = std::collections::HashMap::new();

        // Extract tags from metadata
        
        probed.metadata.get().iter().for_each(|c_metadata| {
            if let Some(o_metadata) = c_metadata.current() {
                for tag in o_metadata.tags() {
                    let key = tag.key.to_lowercase();
                    let value = tag.value.to_string();
                    metadata.insert(key, value);
                }
            }
        });        

        // Extract codec parameters
        let track = match probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        {
            Some(t) => t,
            None => {
                return Err("No decodable audio track found".into());
            }
        };

        let codec_params = &track.codec_params;

        // Add codec information
        if let Some(channels) = codec_params.channels {
            metadata.insert("channels".to_string(), channels.count().to_string());
        }

        if let Some(sample_rate) = codec_params.sample_rate {
            metadata.insert("sample_rate".to_string(), sample_rate.to_string());
        }

        if let Some(n_frames) = codec_params.n_frames {
            metadata.insert("frame_count".to_string(), n_frames.to_string());
            
            if let Some(sample_rate) = codec_params.sample_rate {
                let duration_secs = n_frames as f64 / sample_rate as f64;
                metadata.insert("duration_seconds".to_string(), duration_secs.to_string());
            }
        }

        // Add codec name
        metadata.insert("codec".to_string(), codec_params.codec.to_string());

        Ok(serde_json::to_string(&metadata).map_err(|_| "Failed to serialize metadata")?)
    }

    /// Extracts the first visual thumbnail from the media file, if available. 
    /// Returns the raw image data as a byte vector, or an empty vector if no thumbnail is found.
    pub fn get_thumbnail(&self, path: &str) -> Result<Vec<u8>, String> {
        let single_source: BoxedMediaSource = if path.starts_with("http://") || path.starts_with("https://") {
            let http_stream = HttpStream::new(path)?;
            Box::new(http_stream)
        } else {
            let file = File::open(&path)
                .map_err(|e| format!("Failed to open file source '{path}': {e}"))?;
            Box::new(file)
        };
        let mss = MediaSourceStream::new(single_source, MediaSourceStreamOptions::default());
        let hint = Hint::new();
        let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Failed to probe source for thumbnail: {e}"))?;

        for c_metadata in probed.metadata.get().iter() {
            if let Some(o_metadata) = c_metadata.current() {
                for visual in o_metadata.visuals() {
                    if visual.media_type == "image/jpeg" || visual.media_type == "image/png" {
                        return Ok(visual.data.to_vec());
                    }
                }
            }
        }
        Ok(Vec::new())
    }

    pub fn stop_decode_thread(&mut self) {
        self.decode_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.decode_thread.take() {
            let _ = handle.join();
        }
        self.decode_stop.store(false, Ordering::SeqCst);
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

type BoxedMediaSource = Box<dyn symphonia::core::io::MediaSource>;

fn write_bytes_to_temp_file(bytes: &[u8], tag: &str) -> Result<File, String> {
    let mut path = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    path.push(format!("audiopc_{tag}_{pid}_{nanos}.bin"));

    std::fs::write(&path, bytes)
        .map_err(|e| format!("Failed to write temporary media file '{}': {e}", path.display()))?;

    File::open(&path)
        .map_err(|e| format!("Failed to open temporary media file '{}': {e}", path.display()))
}




/// Creates a MediaSource from the given AudioSource, 
/// which can be a file path, URL, or in-memory data. 
fn media_source_from_owned(source: AudioSource) -> Result<BoxedMediaSource, String> {
    match source {
        AudioSource::Path(path) => {
            let file = File::open(&path)
                .map_err(|e| format!("Failed to open file source '{path}': {e}"))?;
            let source: BoxedMediaSource = Box::new(file);
            Ok(source)
        }
        AudioSource::Url(url) => {
            let stream = HttpStream::new(&url)?;
            let source: BoxedMediaSource = Box::new(stream);
            Ok(source)
        }
        AudioSource::Memory(data) => {
            let file = write_bytes_to_temp_file(&data, "memory")?;
            let source: BoxedMediaSource = Box::new(file);
            Ok(source)
        }
    }
}

/// Similar to media_source_from_owned but takes a reference to avoid cloning large in-memory data.
fn media_source_from_ref(source: &AudioSource) -> Result<BoxedMediaSource, String> {
    match source {
        AudioSource::Path(path) => {
            let file = File::open(path)
                .map_err(|e| format!("Failed to open source for duration: {e}"))?;
            let source: BoxedMediaSource = Box::new(file);
            Ok(source)
        }
        AudioSource::Url(url) => {
            let stream = HttpStream::new(url)?;
            let source: BoxedMediaSource = Box::new(stream);
            Ok(source)
        }
        AudioSource::Memory(data) => {
            let file = write_bytes_to_temp_file(data, "duration_memory")?;
            let source: BoxedMediaSource = Box::new(file);
            Ok(source)
        }
    }
}

fn decode_and_feed(
    source: AudioSource,
    stop_flag: Arc<AtomicBool>,
    shared: Arc<Mutex<SharedPlayback>>,
    out_channels: usize,
    out_sample_rate: u32,
    start_millis: i32,
) -> Result<(), String> {
    let media_source = media_source_from_owned(source)?;

    let mss = MediaSourceStream::new(media_source, MediaSourceStreamOptions::default());

    let hint = Hint::new();
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Failed to probe audio format: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "No decodable audio track found".to_string())?
        .clone();

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Failed to create decoder: {e}"))?;

    let mut resample_state = ResampleState::new();
    let mut skip_output_samples = ((start_millis.max(0) as u64)
        .saturating_mul(out_sample_rate as u64)
        .saturating_mul(out_channels as u64)
        / 1000) as usize;

    let decode_result = loop {
        if stop_flag.load(Ordering::SeqCst) {
            break Ok(());
        }

        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                break Err("Decoder reset required and not supported".to_string())
            }
            Err(SymphoniaError::IoError(_)) => break Ok(()),
            Err(err) => break Err(format!("Failed to read next packet: {err}")),
        };

        let decoded = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => break Ok(()),
            Err(err) => break Err(format!("Failed to decode packet: {err}")),
        };

        let (src_channels, src_rate, interleaved) = decoded_to_interleaved_f32(decoded);
        if interleaved.is_empty() || src_channels == 0 || src_rate == 0 {
            continue;
        }

        if stop_flag.load(Ordering::SeqCst) {
            break Ok(());
        }

        let playback_rate = match shared.lock() {
            Ok(s) => s.playback_rate.clamp(MIN_RATE, MAX_RATE),
            Err(_) => 1.0,
        };

        // Change effective source->output conversion ratio to implement playback speed.
        let effective_out_rate = ((out_sample_rate as f32) / playback_rate).max(1.0) as u32;

        let out = convert_to_output(
            &interleaved,
            src_channels,
            src_rate,
            out_channels,
            effective_out_rate,
            &mut resample_state,
        );

        if out.is_empty() {
            continue;
        }

        let mut start = 0usize;
        if skip_output_samples > 0 {
            let consumed = skip_output_samples.min(out.len());
            skip_output_samples -= consumed;
            start = consumed;
        }

        if start >= out.len() {
            continue;
        }

        let mut offset = 0;
        let out_slice = &out[start..];
        while offset < out_slice.len() {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let pushed = if let Ok(mut s) = shared.lock() {
                s.push_samples_bounded(&out_slice[offset..])
            } else {
                0
            };

            if pushed == 0 {
                thread::sleep(Duration::from_millis(DECODE_BACKPRESSURE_SLEEP_MS));
                continue;
            }

            offset += pushed;
        }
    };

    if let Ok(mut state) = shared.lock() {
        state.stream_finished = true;
    }

    decode_result
}

fn estimate_duration_millis(source: &AudioSource, _out_channels: u32) -> i32 {
    fn from_codec_params(params: &symphonia::core::codecs::CodecParameters) -> i32 {
        if let (Some(n_frames), Some(sample_rate)) = (params.n_frames, params.sample_rate) {
            return ((n_frames as f64 / sample_rate as f64) * 1000.0) as i32;
        }

        -1
    }

    let media_source = match media_source_from_ref(source) {
        Ok(src) => src,
        Err(err) => {
            error!("{err}");
            return -1;
        }
    };

    let mss = MediaSourceStream::new(media_source, MediaSourceStreamOptions::default());
    let hint = Hint::new();
    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(err) => {
            error!("Failed to probe source for duration: {}", err);
            return -1;
        }
    };

    let format = probed.format;
    let track = match format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
    {
        Some(t) => t,
        None => return -1,
    };

    from_codec_params(&track.codec_params)
}

fn decoded_to_interleaved_f32(decoded: AudioBufferRef<'_>) -> (usize, u32, Vec<f32>) {
    let spec = *decoded.spec();
    let channels = spec.channels.count();
    let sample_rate = spec.rate;
    let mut sample_buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    sample_buf.copy_interleaved_ref(decoded);
    (channels, sample_rate, sample_buf.samples().to_vec())
}

#[inline(always)]
fn source_frame_sample(
    frames: &[f32],
    src_channels: usize,
    frame: usize,
    out_channel: usize,
    out_channels: usize,
) -> f32 {
    if src_channels == 0 {
        return 0.0;
    }

    let base = frame * src_channels;

    if out_channels == 1 && src_channels > 1 {
        let mut acc = 0.0;
        for c in 0..src_channels {
            acc += frames[base + c];
        }
        return acc / src_channels as f32;
    }

    if src_channels == 1 {
        return frames[base];
    }

    let idx = out_channel.min(src_channels - 1);
    frames[base + idx]
}

/// Resamples and remaps the source frames to match the output configuration, 
/// using linear interpolation.
fn convert_to_output(
    src_interleaved: &[f32],
    src_channels: usize,
    src_rate: u32,
    out_channels: usize,
    out_rate: u32,
    state: &mut ResampleState,
) -> Vec<f32> {
    if src_channels == 0 || out_channels == 0 || src_rate == 0 || out_rate == 0 {
        return Vec::new();
    }

    let mut frames = Vec::with_capacity(state.carry.len() + src_interleaved.len());
    frames.extend_from_slice(&state.carry);
    frames.extend_from_slice(src_interleaved);

    let total_frames = frames.len() / src_channels;
    if total_frames < 2 {
        state.carry = frames;
        return Vec::new();
    }

    let step = src_rate as f64 / out_rate as f64;
    let mut pos = state.pos;

    let estimated_frames = (((total_frames as f64 - pos - 1.0) / step)
        .max(0.0)
        .ceil() as usize)
        .saturating_add(1);
    let mut out = Vec::with_capacity(estimated_frames.saturating_mul(out_channels));

    while pos + 1.0 < total_frames as f64 {
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;

        for ch in 0..out_channels {
            let s0 = source_frame_sample(&frames, src_channels, i0, ch, out_channels);
            let s1 = source_frame_sample(&frames, src_channels, i0 + 1, ch, out_channels);
            out.push(s0 + (s1 - s0) * frac);
        }

        pos += step;
    }

    let keep_frame = total_frames - 1;
    let keep_base = keep_frame * src_channels;
    state.carry.clear();
    state
        .carry
        .extend_from_slice(&frames[keep_base..keep_base + src_channels]);
    state.pos = pos - keep_frame as f64;

    out
}

/// Writes the next audio samples to the output buffer, applying volume and handling synchronization.
fn write_output_f32(data: &mut [f32], channels: usize, shared: &Arc<Mutex<SharedPlayback>>) {
    let mut guard = match shared.lock() {
        Ok(g) => g,
        Err(_) => {
            data.fill(0.0);
            return;
        }
    };

    for frame in data.chunks_mut(channels) {
        for (_, out) in frame.iter_mut().enumerate() {
            *out = guard.next_sample();
        }
    }
}

/// Similar to write_output_f32 but converts the samples to i16 format, scaling appropriately.
fn write_output_i16(data: &mut [i16], channels: usize, shared: &Arc<Mutex<SharedPlayback>>) {
    let mut guard = match shared.lock() {
        Ok(g) => g,
        Err(_) => {
            data.fill(0);
            return;
        }
    };

    for frame in data.chunks_mut(channels) {
        for (_, out) in frame.iter_mut().enumerate() {
            let sample = guard.next_sample();
            *out = (sample * i16::MAX as f32) as i16;
        }
    }
}

/// Similar to write_output_f32 but converts the samples to u16 format, scaling and offsetting to fit the unsigned range.
fn write_output_u16(data: &mut [u16], channels: usize, shared: &Arc<Mutex<SharedPlayback>>) {
    let mut guard = match shared.lock() {
        Ok(g) => g,
        Err(_) => {
            data.fill(u16::MAX / 2);
            return;
        }
    };

    for frame in data.chunks_mut(channels) {
        for (_, out) in frame.iter_mut().enumerate() {
            let sample = guard.next_sample();
            *out = (((sample * 0.5) + 0.5) * u16::MAX as f32) as u16;
        }
    }
}

pub fn default_output_sample_rate() -> i32 {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        return -1;
    };

    match device.default_output_config() {
        Ok(config) => config.sample_rate() as i32,
        Err(_) => -1,
    }
}

pub fn default_output_channels() -> i32 {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        return -1;
    };

    match device.default_output_config() {
        Ok(config) => i32::from(config.channels()),
        Err(_) => -1,
    }
}

pub fn output_device_count() -> i32 {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.count() as i32,
        Err(_) => -1,
    }
}