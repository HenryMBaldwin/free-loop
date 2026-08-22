//! The metronome click.
//!
//! A synthesised blip rather than a sample. Beat one is pitched higher and runs slightly
//! longer than the others.

use free_loop_core::SampleRate;

use core::f32::consts::TAU;

/// Frequency of the downbeat blip, in hertz.
const ACCENT_HZ: f32 = 1_600.0;
/// Frequency of the other beats, in hertz.
const BEAT_HZ: f32 = 900.0;
/// Frequency of a blip between beats, in hertz.
const SUB_HZ: f32 = 660.0;
/// Duration of the downbeat blip, in milliseconds.
const ACCENT_MS: f32 = 28.0;
/// Duration of the other beats, in milliseconds.
const BEAT_MS: f32 = 18.0;
/// Duration of a blip between beats, in milliseconds.
const SUB_MS: f32 = 12.0;

/// Which blip to sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Beat one.
    Accent,
    /// Any other beat.
    Beat,
    /// Between beats.
    Sub,
}

impl Tone {
    /// Pitch in hertz and length in milliseconds.
    fn voice(self) -> (f32, f32) {
        match self {
            Self::Accent => (ACCENT_HZ, ACCENT_MS),
            Self::Beat => (BEAT_HZ, BEAT_MS),
            Self::Sub => (SUB_HZ, SUB_MS),
        }
    }
}

/// How the click starts up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickConfig {
    /// Whether the click sounds.
    pub enabled: bool,
    /// Peak amplitude, `0.0..=1.0`.
    pub level: f32,
}

impl Default for ClickConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: 0.35,
        }
    }
}

/// Generates the click. Adds into the output; never allocates.
#[derive(Debug)]
pub struct Click {
    enabled: bool,
    level: f32,
    sample_rate: f32,
    /// Oscillator phase in radians.
    phase: f32,
    /// Radians per frame for the blip currently sounding.
    step: f32,
    /// Frames left in the current blip.
    remaining: u32,
    /// Length of the current blip, for the envelope.
    length: u32,
}

impl Click {
    /// Builds a silent click generator.
    pub fn new(config: ClickConfig, sample_rate: SampleRate) -> Self {
        #[expect(
            clippy::cast_precision_loss,
            reason = "sample rates are well inside f32's exact integer range"
        )]
        let hz = sample_rate.hz() as f32;
        Self {
            enabled: config.enabled,
            level: config.level.clamp(0.0, 1.0),
            sample_rate: hz,
            phase: 0.0,
            step: 0.0,
            remaining: 0,
            length: 1,
        }
    }

    /// Whether the click sounds.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns the click on or off. Cuts any blip still sounding.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.remaining = 0;
        }
    }

    /// Sets the peak amplitude, clamped to `0.0..=1.0`.
    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    /// Starts a blip in the voice `tone` calls for.
    pub fn trigger(&mut self, tone: Tone) {
        if !self.enabled {
            return;
        }
        let (hz, millis) = tone.voice();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a positive blip length of a few thousand frames"
        )]
        let length = ((millis / 1000.0) * self.sample_rate).max(1.0) as u32;
        self.length = length;
        self.remaining = length;
        self.step = TAU * hz / self.sample_rate;
        self.phase = 0.0;
    }

    /// Whether a blip is still sounding.
    pub fn is_sounding(&self) -> bool {
        self.remaining > 0
    }

    /// Adds the click into `dst`, which is interleaved across `channels`.
    pub fn add_into(&mut self, dst: &mut [f32], channels: usize) {
        if self.remaining == 0 || self.level <= 0.0 {
            return;
        }

        for frame in dst.chunks_mut(channels) {
            if self.remaining == 0 {
                break;
            }
            // Squared linear ramp: a cheap decay that reaches exactly zero, so the blip
            // never clicks on its own tail.
            #[expect(
                clippy::cast_precision_loss,
                reason = "blip lengths are a few thousand frames"
            )]
            let fraction = self.remaining as f32 / self.length as f32;
            let sample = self.level * fraction * fraction * self.phase.sin();

            for out in frame.iter_mut() {
                *out += sample;
            }

            self.phase += self.step;
            if self.phase >= TAU {
                self.phase -= TAU;
            }
            self.remaining -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        reason = "tests should fail loudly, and compare exact synthesised values"
    )]

    use super::*;

    fn click() -> Click {
        Click::new(ClickConfig::default(), SampleRate::new(48_000).unwrap())
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
    }

    #[test]
    fn a_silent_generator_leaves_the_output_alone() {
        let mut click = click();
        let mut out = vec![0.0; 128];
        click.add_into(&mut out, 2);
        assert_eq!(peak(&out), 0.0);
    }

    #[test]
    fn a_blip_sounds_then_stops() {
        let mut click = click();
        click.trigger(Tone::Beat);
        assert!(click.is_sounding());

        // 18 ms at 48 kHz is 864 frames.
        let mut out = vec![0.0; 864 * 2];
        click.add_into(&mut out, 2);
        assert!(peak(&out) > 0.0);
        assert!(!click.is_sounding());

        let mut after = vec![0.0; 128];
        click.add_into(&mut after, 2);
        assert_eq!(peak(&after), 0.0);
    }

    #[test]
    fn the_envelope_decays_to_zero() {
        let mut click = click();
        click.trigger(Tone::Accent);
        let mut out = vec![0.0; 4_000 * 2];
        click.add_into(&mut out, 2);

        // 28 ms at 48 kHz is 1344 frames, so the tail is silent well before frame 2000.
        let first = peak(&out[..200]);
        let last = peak(&out[2_000 * 2..]);
        assert!(first > last, "{first} should exceed {last}");
        assert_eq!(last, 0.0, "the blip must end in silence");
    }

    #[test]
    fn the_accent_is_longer_than_the_other_beats() {
        let mut accented = click();
        accented.trigger(Tone::Accent);
        let mut plain = click();
        plain.trigger(Tone::Beat);

        let mut a = vec![0.0; 4_000 * 2];
        let mut b = vec![0.0; 4_000 * 2];
        accented.add_into(&mut a, 2);
        plain.add_into(&mut b, 2);

        let tail = |v: &[f32]| v.iter().rposition(|s| *s != 0.0).unwrap_or(0);
        assert!(tail(&a) > tail(&b));
    }

    #[test]
    fn the_click_is_written_to_every_channel() {
        let mut click = click();
        click.trigger(Tone::Accent);
        let mut out = vec![0.0; 64 * 2];
        click.add_into(&mut out, 2);

        for frame in out.chunks(2) {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn disabling_cuts_a_sounding_blip() {
        let mut click = click();
        click.trigger(Tone::Accent);
        click.set_enabled(false);
        assert!(!click.is_sounding());

        click.trigger(Tone::Accent);
        assert!(!click.is_sounding(), "a disabled click cannot be triggered");
    }

    #[test]
    fn level_is_clamped_and_applied() {
        let mut click = click();
        click.set_level(10.0);
        click.trigger(Tone::Accent);
        let mut out = vec![0.0; 2_000 * 2];
        click.add_into(&mut out, 2);
        assert!(peak(&out) <= 1.0);

        let mut quiet = click;
        quiet.set_level(0.0);
        quiet.trigger(Tone::Accent);
        let mut out = vec![0.0; 2_000 * 2];
        quiet.add_into(&mut out, 2);
        assert_eq!(peak(&out), 0.0);
    }
}
