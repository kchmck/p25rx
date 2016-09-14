/// Number of SDR sample byffers to allocate.
pub const BUF_COUNT: usize = 16;
/// Size of each SDR sample buffer (bytes).
pub const BUF_SIZE_RAW: usize = 32768;
/// Number of samples after transforming byte pairs to complex samples.
pub const BUF_SIZE_COMPLEX: usize = BUF_SIZE_RAW / 2;

/// Sample rate for the SDR.
pub const SDR_SAMPLE_RATE: u32 = 240_000;
/// Downconverted baseband sample rate.
pub const BASEBAND_SAMPLE_RATE: u32 = 48000;

/// Default site to use from configuration.
pub const DEFAULT_SITE: usize = 0;
