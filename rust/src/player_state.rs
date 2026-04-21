use std::collections::VecDeque;

use crate::{effects::Effects, enums::{DEFAULT_MAX_QUEUE_SECONDS, DEFAULT_VISUALIZER_SECONDS, MAX_MAX_QUEUE_SECONDS, MIN_MAX_QUEUE_SECONDS}};

/// The `PlayerState` enum is used to represent the current state of the player, 
/// such as whether it is idle, playing, paused, or stopped. 
/// This can be useful for updating UI elements or implementing custom playback logic based on the current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
  Idle,
  Playing,
  Paused,
  Stopped,
}

/// ResampleState holds the state for resampling audio when the playback rate is changed.
/// It includes the current position in the resampling process and any carry-over samples 
/// that need to be processed in the next callback.
/// This allows for smooth time-stretching of the audio when the playback rate is adjusted, 
/// ensuring that the output remains consistent and free of artifacts.
pub struct ResampleState {
   pub pos: f64,
   pub carry: Vec<f32>,
}

impl ResampleState {
   pub fn new() -> Self {
        Self {
            pos: 0.0,
            carry: Vec::new(),
        }
    }
}

/// The shared playback state that is accessed by both the audio callback and the decode thread.
/// This struct contains the main audio sample queue, the visualizer ring buffer, and various playback parameters. 
/// It is protected by a mutex to ensure thread safety when accessed from multiple threads.
pub struct SharedPlayback {
   /// The main audio sample queue that feeds the output. 
   /// This queue is filled by the decode thread and consumed by the audio callback.
   pub queue: VecDeque<f32>,

   /// A ring buffer that holds the most recent samples for visualizer purposes. 
   /// This allows the visualizer to access a snapshot of the latest audio data without interfering with the main playback queue.
   pub visualizer_ring: VecDeque<f32>,

   /// The maximum number of samples to keep in the visualizer ring buffer. 
   /// This is calculated based on the sample rate, number of channels, and a configurable duration (e.g., 5 seconds).
   pub visualizer_max_samples: usize,

   /// The maximum number of samples to keep in the main playback queue. 
   /// This is calculated based on the sample rate, number of channels, and a configurable duration (e.g., 30 seconds).
   pub max_samples: usize,

   /// The maximum number of seconds worth of audio to keep in the main playback queue. 
   /// This is a user-configurable setting that determines how much audio data can be buffered for playback.
   pub max_queue_seconds: usize,

   /// The current playback rate. A value of 1.0 means normal speed, 0.5 means half speed, and 2.0 means double speed. 
   /// This can be used to implement features like slow motion or fast forward.
   /// Note that changing the playback rate may affect the pitch of the audio unless time-stretching algorithms are used.
   pub playback_rate: f32,

   /// Indicates whether the player is currently playing.
   pub playing: bool,

   /// Indicates whether the audio stream has finished playing. 
   /// This is set to true when the end of the audio source is reached, 
   /// allowing the player to stop playback and reset the state as needed.
   pub stream_finished: bool,

   /// The current volume level, where 1.0 is the original volume, 0.5 is half volume, and 2.0 is double volume. 
   /// This can be used to implement volume control features in the player.
   pub volume: f32,

   /// The total number of audio samples that have been emitted to the output. 
   /// This is used to track the playback position and can be useful for features like seeking or displaying the current time in the UI.
   /// Note that this count is based on the number of samples sent to the output, not the number of samples decoded or processed.
   /// It is incremented in the audio callback each time a sample is output, and it can be used to calculate the current playback time by dividing by the sample rate.
   pub emitted_samples: u64,

   /// The offset in samples from the start of the audio source. 
   /// This is used to track the current position in the audio source, especially when seeking or when starting playback from a specific point.
   /// When a new audio source is loaded, this offset is reset to zero. As playback progresses, this offset is updated based on the number of samples emitted and any seeking actions taken by the user.
   /// This allows the player to maintain an accurate position within the audio source, which is essential for features like seeking, displaying the current time, and synchronizing with visualizers or other components.
   /// Note that this offset is separate from the emitted_samples count, as it represents the position within the source rather than the total number of samples output. It is used in conjunction with the emitted_samples to calculate the current playback position and to manage seeking and other position-related features.
   pub source_offset_samples: u64,

   /// The sample rate of the audio being played. This is used to calculate timing and to manage the playback queue and visualizer buffers. 
   /// The sample rate is typically determined by the audio source and the output device, and it can affect the quality and performance of the audio playback. 
   /// Common sample rates include 44100 Hz (CD quality), 48000 Hz (DVD quality), and 96000 Hz (high-resolution audio).
   /// The sample rate is used in various calculations throughout the player, such as determining how many samples correspond to a certain duration of time, managing the visualizer buffer size, and ensuring that the audio is processed correctly for the output device. It is an essential parameter for accurate audio playback and synchronization with visualizers and other components.
   pub sample_rate: u32,

   /// A vector of effects processors, one for each output channel. 
   /// These processors can apply various audio effects (e.g., reverb, delay, distortion) to the output samples before they are sent to the audio output. 
   /// Each effect processor can be configured independently, allowing for different effects on different channels if desired
   pub effects: Vec<Effects>,
}

impl SharedPlayback {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let max_samples = sample_rate
            .saturating_mul(channels as u32)
            .saturating_mul(DEFAULT_MAX_QUEUE_SECONDS as u32) as usize;
        let visualizer_max_samples = sample_rate
            .saturating_mul(channels as u32)
            .saturating_mul(DEFAULT_VISUALIZER_SECONDS as u32) as usize;
        Self {
            queue: VecDeque::with_capacity(max_samples),
            visualizer_ring: VecDeque::with_capacity(visualizer_max_samples),
            visualizer_max_samples,
            max_samples,
            max_queue_seconds: DEFAULT_MAX_QUEUE_SECONDS,
            playback_rate: 1.0,
            playing: false,
            stream_finished: true,
            volume: 1.0,
            emitted_samples: 0,
            source_offset_samples: 0,
            sample_rate,
            effects: (0..channels).map(|_| Effects::new()).collect(),
        }
    }

    pub fn clear_audio_state(&mut self) {
        self.queue.clear();
        self.visualizer_ring.clear();
        self.emitted_samples = 0;
        self.source_offset_samples = 0;
        self.stream_finished = true;
        for effect in &mut self.effects {
            *effect = Effects::new();
        }
    }

    pub fn push_visualizer_sample(&mut self, sample: f32) {
        if self.visualizer_max_samples == 0 {
            return;
        }

        if self.visualizer_ring.len() >= self.visualizer_max_samples {
            let _ = self.visualizer_ring.pop_front();
        }

        self.visualizer_ring.push_back(sample);
    }

    pub fn copy_latest_visualizer_samples(&self, out: &mut [f32]) -> usize {
        if out.is_empty() || self.visualizer_ring.is_empty() {
            return 0;
        }

        let count = out.len().min(self.visualizer_ring.len());
        let skip = self.visualizer_ring.len().saturating_sub(count);

        for (index, sample) in self
            .visualizer_ring
            .iter()
            .skip(skip)
            .take(count)
            .enumerate()
        {
            out[index] = *sample;
        }

        count
    }

    pub fn set_max_queue_seconds(&mut self, channels: usize, seconds: usize) {
        let bounded = seconds.clamp(MIN_MAX_QUEUE_SECONDS, MAX_MAX_QUEUE_SECONDS);
        self.max_queue_seconds = bounded;
        self.max_samples = self
            .sample_rate
            .saturating_mul(channels as u32)
            .saturating_mul(bounded as u32) as usize;

        while self.queue.len() > self.max_samples {
            let _ = self.queue.pop_front();
        }

        if self.queue.capacity() < self.max_samples {
            self.queue
                .reserve(self.max_samples.saturating_sub(self.queue.capacity()));
        }
    }

    pub fn push_samples_bounded(&mut self, samples: &[f32]) -> usize {
        let available = self.max_samples.saturating_sub(self.queue.len());
        let push_count = samples.len().min(available);
        self.queue.extend(samples.iter().take(push_count).copied());
        push_count
    }

    pub fn next_sample(&mut self) -> f32 {
        if !self.playing {
            return 0.0;
        }

        let raw = if let Some(sample) = self.queue.pop_front() {
            self.emitted_samples = self.emitted_samples.saturating_add(1);
            sample
        } else {
            if self.stream_finished {
                self.playing = false;
            }
            return 0.0;
        };

        let mut sample = raw * self.volume;

        for effect in &mut self.effects {
            sample = effect.process(sample);
        }

        let sample = sample.clamp(-1.0, 1.0);
        self.push_visualizer_sample(sample);
        sample
    }
}