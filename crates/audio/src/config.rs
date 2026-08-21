//! Choosing a stream configuration from what a device offers.
//!
//! Works on `SupportedStreamConfigRange` values rather than on an open device.

use cpal::{
    BufferSize, SampleFormat, SupportedBufferSize, SupportedStreamConfig,
    SupportedStreamConfigRange,
};

/// Sample rates preferred when the caller does not ask for one, in order.
pub const PREFERRED_RATES: [u32; 2] = [48_000, 44_100];

/// Channel count assumed when the caller does not ask for one.
pub const DEFAULT_CHANNELS: u16 = 2;

/// Block size assumed when the device will not say what it uses.
pub const ASSUMED_BLOCK_FRAMES: u32 = 512;

/// What the caller wants from the audio devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioConfig {
    /// Substring of the input device name. `None` uses the default device.
    pub input_device: Option<String>,
    /// Substring of the output device name. `None` uses the default device.
    pub output_device: Option<String>,
    /// Sample rate to request. `None` takes the device's preference.
    pub sample_rate: Option<u32>,
    /// Block size to request. `None` leaves it to the device.
    pub buffer_frames: Option<u32>,
    /// Channel count to request. `None` asks for stereo.
    pub channels: Option<u16>,
    /// Blocks of capture to buffer before the output starts consuming.
    ///
    /// This is the fixed delay between the input and output callbacks, and it is the
    /// part of the round trip the software controls.
    pub cushion_blocks: u32,
    /// Round-trip latency to compensate for, in frames.
    ///
    /// `None` measures it from the driver's own callback timestamps. Set it only to
    /// override a driver that reports badly.
    pub capture_offset: Option<u32>,
}

impl AudioConfig {
    /// Defaults: default devices, stereo, device-chosen rate, two blocks of cushion.
    pub fn new() -> Self {
        Self {
            cushion_blocks: 2,
            ..Self::default()
        }
    }
}

/// The configuration both streams agreed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// Rate both streams run at.
    pub sample_rate: u32,
    /// Channels the engine works in. This is the output channel count.
    pub channels: usize,
    /// Channels the input device delivers, before mapping.
    pub input_channels: usize,
    /// Sample format the input device delivers.
    pub input_format: SampleFormat,
    /// Sample format the output device expects.
    pub output_format: SampleFormat,
    /// Channels the engine reads, which is the device's width up to the ceiling.
    pub capture_channels: usize,
    /// Block size requested, if one was.
    pub buffer_frames: Option<u32>,
    /// Frames of capture buffered before the output starts consuming.
    pub cushion_frames: usize,
    /// Round-trip latency the caller pinned, if any.
    pub capture_offset: Option<u32>,
}

impl Negotiated {
    /// Frames of latency this crate adds on top of the device's own round trip.
    pub fn added_latency_frames(&self) -> usize {
        self.cushion_frames
    }
}

/// Converts a duration reported by a driver into frames.
pub fn frames_in(duration: core::time::Duration, sample_rate: u32) -> u32 {
    let frames = duration.as_nanos() * u128::from(sample_rate) / 1_000_000_000;
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// Formats this crate can convert, best first. Anything else is refused rather than
/// silently mangled.
fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::I32 => Some(2),
        SampleFormat::U16 => Some(3),
        _ => None,
    }
}

/// How far a rate misses.
///
/// With no request, rates are ranked by position in [`PREFERRED_RATES`]. `CoreAudio`
/// reports each discrete rate as its own single-rate range, so preferring 48 kHz
/// within a range never gets to choose between them. Rates outside the list rank
/// below every rate in it, closest to the top preference first.
fn rate_penalty(rate: u32, wanted: Option<u32>) -> u32 {
    if let Some(wanted) = wanted {
        return wanted.abs_diff(rate);
    }
    if let Some(index) = PREFERRED_RATES
        .iter()
        .position(|&preferred| preferred == rate)
    {
        return u32::try_from(index).unwrap_or(u32::MAX);
    }
    let tier = u32::try_from(PREFERRED_RATES.len()).unwrap_or(u32::MAX);
    tier.saturating_add(rate.abs_diff(PREFERRED_RATES[0]))
}

/// The rate to use from a range, given what was asked for.
fn pick_rate(range: &SupportedStreamConfigRange, wanted: Option<u32>) -> u32 {
    let min = range.min_sample_rate();
    let max = range.max_sample_rate();

    if let Some(rate) = wanted {
        return rate.clamp(min, max);
    }
    for preferred in PREFERRED_RATES {
        if (min..=max).contains(&preferred) {
            return preferred;
        }
    }
    max
}

/// How badly a channel count misses. Extra channels are usable, since the mapper takes
/// the first few, so missing channels is the far worse outcome.
fn channel_penalty(have: u16, want: u16) -> u32 {
    if have >= want {
        u32::from(have - want)
    } else {
        1_000 + u32::from(want - have)
    }
}

/// Picks the best configuration a device offers.
///
/// Ranked by channel fit first, then how close the rate lands to the request, then
/// format preference, then fewest channels.
pub fn choose(
    ranges: &[SupportedStreamConfigRange],
    wanted_rate: Option<u32>,
    wanted_channels: Option<u16>,
) -> Option<SupportedStreamConfig> {
    let want_channels = wanted_channels.unwrap_or(DEFAULT_CHANNELS);

    ranges
        .iter()
        .filter_map(|range| Some((range, format_rank(range.sample_format())?)))
        .map(|(range, rank)| {
            let rate = pick_rate(range, wanted_rate);
            let key = (
                channel_penalty(range.channels(), want_channels),
                rate_penalty(rate, wanted_rate),
                rank,
                range.channels(),
            );
            (key, range, rate)
        })
        .min_by_key(|(key, _, _)| *key)
        .map(|(_, range, rate)| {
            SupportedStreamConfig::new(
                range.channels(),
                rate,
                *range.buffer_size(),
                range.sample_format(),
            )
        })
}

/// Turns a requested block size into one the device accepts.
pub fn buffer_size(supported: &SupportedBufferSize, wanted: Option<u32>) -> BufferSize {
    match (supported, wanted) {
        (SupportedBufferSize::Range { min, max }, Some(frames)) => {
            BufferSize::Fixed(frames.clamp(*min, *max))
        }
        _ => BufferSize::Default,
    }
}

/// Frames of cushion for a given block size and block count.
pub fn cushion_frames(buffer_frames: Option<u32>, blocks: u32) -> usize {
    let block = buffer_frames.unwrap_or(ASSUMED_BLOCK_FRAMES).max(1);
    usize::try_from(block.saturating_mul(blocks)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests should fail loudly")]

    use super::*;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            min,
            max,
            SupportedBufferSize::Range { min: 64, max: 4096 },
            format,
        )
    }

    #[test]
    fn f32_wins_when_everything_else_is_equal() {
        let ranges = [
            range(2, 48_000, 48_000, SampleFormat::I16),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ];
        let chosen = choose(&ranges, None, Some(2)).unwrap();
        assert_eq!(chosen.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn the_requested_rate_is_taken_when_the_device_covers_it() {
        let ranges = [range(2, 44_100, 96_000, SampleFormat::F32)];
        assert_eq!(
            choose(&ranges, Some(88_200), None).unwrap().sample_rate(),
            88_200
        );
    }

    #[test]
    fn a_rate_outside_the_range_clamps_rather_than_failing() {
        let ranges = [range(2, 44_100, 48_000, SampleFormat::F32)];
        assert_eq!(
            choose(&ranges, Some(192_000), None).unwrap().sample_rate(),
            48_000
        );
    }

    #[test]
    fn without_a_request_48k_is_preferred_then_44k1() {
        let wide = [range(2, 8_000, 96_000, SampleFormat::F32)];
        assert_eq!(choose(&wide, None, None).unwrap().sample_rate(), 48_000);

        let narrow = [range(2, 8_000, 44_100, SampleFormat::F32)];
        assert_eq!(choose(&narrow, None, None).unwrap().sample_rate(), 44_100);

        let odd = [range(2, 32_000, 32_000, SampleFormat::F32)];
        assert_eq!(choose(&odd, None, None).unwrap().sample_rate(), 32_000);
    }

    /// `CoreAudio` reports one range per discrete rate, so the preference has to work
    /// across ranges rather than inside one. Found on a Scarlett Solo, which was landing
    /// on 44.1 kHz purely because that range was enumerated first.
    #[test]
    fn single_rate_ranges_still_prefer_48k() {
        let ranges = [
            range(2, 44_100, 44_100, SampleFormat::F32),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ];
        assert_eq!(choose(&ranges, None, None).unwrap().sample_rate(), 48_000);

        // Order must not matter.
        let reversed = [ranges[1], ranges[0]];
        assert_eq!(choose(&reversed, None, None).unwrap().sample_rate(), 48_000);
    }

    #[test]
    fn an_unlisted_rate_loses_to_a_preferred_one() {
        let ranges = [
            range(2, 96_000, 96_000, SampleFormat::F32),
            range(2, 44_100, 44_100, SampleFormat::F32),
        ];
        assert_eq!(choose(&ranges, None, None).unwrap().sample_rate(), 44_100);
    }

    #[test]
    fn among_unlisted_rates_the_closest_to_48k_wins() {
        let ranges = [
            range(2, 192_000, 192_000, SampleFormat::F32),
            range(2, 32_000, 32_000, SampleFormat::F32),
        ];
        assert_eq!(choose(&ranges, None, None).unwrap().sample_rate(), 32_000);
    }

    #[test]
    fn an_explicit_request_still_outranks_the_preference() {
        let ranges = [
            range(2, 48_000, 48_000, SampleFormat::F32),
            range(2, 44_100, 44_100, SampleFormat::F32),
        ];
        assert_eq!(
            choose(&ranges, Some(44_100), None).unwrap().sample_rate(),
            44_100
        );
    }

    #[test]
    fn extra_channels_beat_missing_ones() {
        let ranges = [
            range(1, 48_000, 48_000, SampleFormat::F32),
            range(8, 48_000, 48_000, SampleFormat::F32),
        ];
        assert_eq!(choose(&ranges, None, Some(2)).unwrap().channels(), 8);
    }

    #[test]
    fn an_exact_channel_match_is_preferred_to_a_wider_device() {
        let ranges = [
            range(8, 48_000, 48_000, SampleFormat::F32),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ];
        assert_eq!(choose(&ranges, None, Some(2)).unwrap().channels(), 2);
    }

    #[test]
    fn the_channel_fit_outranks_the_sample_format() {
        let ranges = [
            range(1, 48_000, 48_000, SampleFormat::F32),
            range(2, 48_000, 48_000, SampleFormat::I16),
        ];
        let chosen = choose(&ranges, None, Some(2)).unwrap();
        assert_eq!(chosen.channels(), 2);
        assert_eq!(chosen.sample_format(), SampleFormat::I16);
    }

    #[test]
    fn formats_this_crate_cannot_convert_are_refused() {
        let ranges = [range(2, 48_000, 48_000, SampleFormat::I24)];
        assert!(choose(&ranges, None, None).is_none());
        assert!(choose(&[], None, None).is_none());
    }

    #[test]
    fn a_requested_block_size_is_clamped_into_range() {
        let supported = SupportedBufferSize::Range { min: 64, max: 1024 };
        assert_eq!(buffer_size(&supported, Some(256)), BufferSize::Fixed(256));
        assert_eq!(buffer_size(&supported, Some(16)), BufferSize::Fixed(64));
        assert_eq!(buffer_size(&supported, Some(8192)), BufferSize::Fixed(1024));
        assert_eq!(buffer_size(&supported, None), BufferSize::Default);
        assert_eq!(
            buffer_size(&SupportedBufferSize::Unknown, Some(256)),
            BufferSize::Default
        );
    }

    #[test]
    fn driver_durations_become_frames() {
        use core::time::Duration;

        assert_eq!(frames_in(Duration::ZERO, 48_000), 0);
        assert_eq!(frames_in(Duration::from_millis(10), 48_000), 480);
        assert_eq!(frames_in(Duration::from_secs(1), 44_100), 44_100);
        // Sub-frame durations round down rather than inventing a frame.
        assert_eq!(frames_in(Duration::from_nanos(1), 48_000), 0);
    }

    #[test]
    fn the_cushion_falls_back_to_an_assumed_block() {
        assert_eq!(cushion_frames(Some(128), 2), 256);
        assert_eq!(cushion_frames(None, 2), 1_024);
        assert_eq!(cushion_frames(Some(0), 2), 2);
    }
}
