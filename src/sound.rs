//! Small synthesized notification cues.
//!
//! Cues are generated on demand instead of embedded as assets, keeping the
//! binary and release archives small. Every style has a positive completion
//! cue and a distinct attention cue for blocked agents.

use serde::{Deserialize, Serialize};

pub const STYLE_RETRO: &str = "retro";
pub const STYLE_SOFT: &str = "soft";
pub const STYLE_PULSE: &str = "pulse";
pub const STYLES: [SoundStyle; 3] = [SoundStyle::Retro, SoundStyle::Soft, SoundStyle::Pulse];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SoundCue {
    Done,
    Blocked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SoundStyle {
    Retro,
    Soft,
    Pulse,
}

impl SoundStyle {
    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            STYLE_SOFT => Self::Soft,
            STYLE_PULSE => Self::Pulse,
            _ => Self::Retro,
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Self::Retro => STYLE_RETRO,
            Self::Soft => STYLE_SOFT,
            Self::Pulse => STYLE_PULSE,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Retro => "Retro",
            Self::Soft => "Soft",
            Self::Pulse => "Pulse",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SoundSignal {
    pub cue: SoundCue,
    pub style: SoundStyle,
}

#[derive(Clone, Copy)]
enum Waveform {
    Square,
    Triangle,
    Pulse,
}

const SAMPLE_RATE: u32 = 22_050;

/// Synthesize one complete notification cue as 16-bit mono PCM WAV.
pub fn synth_wav(signal: SoundSignal) -> Vec<u8> {
    let mut samples = Vec::new();
    match (signal.style, signal.cue) {
        // Preserve the original Luvus completion jingle exactly.
        (SoundStyle::Retro, SoundCue::Done) => {
            for (index, frequency) in [523.25, 659.25, 783.99, 1046.5].into_iter().enumerate() {
                let duration = if index == 3 { 180 } else { 90 };
                push_tone(&mut samples, frequency, duration, 0.22, Waveform::Square);
            }
        }
        (SoundStyle::Retro, SoundCue::Blocked) => {
            push_tone(&mut samples, 392.0, 120, 0.20, Waveform::Square);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 293.66, 180, 0.20, Waveform::Square);
        }
        (SoundStyle::Soft, SoundCue::Done) => {
            push_tone(&mut samples, 523.25, 150, 0.14, Waveform::Triangle);
            push_tone(&mut samples, 783.99, 230, 0.14, Waveform::Triangle);
        }
        (SoundStyle::Soft, SoundCue::Blocked) => {
            push_tone(&mut samples, 440.0, 125, 0.13, Waveform::Triangle);
            push_silence(&mut samples, 90);
            push_tone(&mut samples, 440.0, 125, 0.13, Waveform::Triangle);
        }
        (SoundStyle::Pulse, SoundCue::Done) => {
            push_tone(&mut samples, 659.25, 70, 0.19, Waveform::Pulse);
            push_silence(&mut samples, 35);
            push_tone(&mut samples, 987.77, 120, 0.19, Waveform::Pulse);
        }
        (SoundStyle::Pulse, SoundCue::Blocked) => {
            push_tone(&mut samples, 329.63, 85, 0.20, Waveform::Pulse);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 329.63, 85, 0.20, Waveform::Pulse);
            push_silence(&mut samples, 55);
            push_tone(&mut samples, 329.63, 120, 0.20, Waveform::Pulse);
        }
    }
    wav_bytes(&samples, SAMPLE_RATE)
}

fn push_tone(
    samples: &mut Vec<i16>,
    frequency: f32,
    duration_ms: u32,
    amplitude: f32,
    waveform: Waveform,
) {
    let count = SAMPLE_RATE * duration_ms / 1000;
    let fade = (SAMPLE_RATE / 200).max(1);
    for index in 0..count {
        let phase = (index as f32 * frequency / SAMPLE_RATE as f32) % 1.0;
        let wave = match waveform {
            Waveform::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
            Waveform::Pulse => {
                if phase < 0.22 {
                    1.0
                } else {
                    -0.35
                }
            }
        };
        let fade_in = index.min(fade) as f32 / fade as f32;
        let fade_out = count.saturating_sub(index).min(fade) as f32 / fade as f32;
        samples.push((wave * amplitude * fade_in.min(fade_out) * i16::MAX as f32) as i16);
    }
}

fn push_silence(samples: &mut Vec<i16>, duration_ms: u32) {
    samples.resize(
        samples.len() + (SAMPLE_RATE * duration_ms / 1000) as usize,
        0,
    );
}

fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_has_distinct_valid_done_and_blocked_wavs() {
        for style in STYLES {
            let done = synth_wav(SoundSignal {
                cue: SoundCue::Done,
                style,
            });
            let blocked = synth_wav(SoundSignal {
                cue: SoundCue::Blocked,
                style,
            });
            for wav in [&done, &blocked] {
                assert_eq!(&wav[0..4], b"RIFF");
                assert_eq!(&wav[8..12], b"WAVE");
                assert_eq!(&wav[12..16], b"fmt ");
                assert_eq!(&wav[36..40], b"data");
                let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
                assert_eq!(wav.len(), 44 + data_len);
                assert!(data_len > 0);
            }
            assert_ne!(done, blocked, "{} cues must be recognizable", style.label());
        }
    }

    #[test]
    fn unknown_style_falls_back_without_rejecting_config() {
        assert_eq!(SoundStyle::from_config("future-style"), SoundStyle::Retro);
    }
}
