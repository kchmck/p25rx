/// Number of SDR sample byffers to allocate.
pub const BUF_COUNT: usize = 16;
/// Size of each SDR sample buffer (bytes).
pub const BUF_BYTES: usize = 32768;
/// Number of samples after transforming byte pairs to complex samples.
pub const BUF_SAMPLES: usize = BUF_BYTES / 2;

/// Sample rate for the SDR.
pub const SDR_SAMPLE_RATE: u32 = 240000;
/// Downconverted baseband sample rate.
pub const BASEBAND_SAMPLE_RATE: u32 = 48000;

#[cfg(test)]
mod test {
    use p25;

    #[test]
    fn verify_sample_rate() {
        assert_eq!(super::BASEBAND_SAMPLE_RATE as usize, p25::consts::SAMPLE_RATE);
    }
}
