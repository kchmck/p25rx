//! Demodulation and other signal processing.

use std::sync::mpsc::{Sender, Receiver};
use std;

use collect_slice::CollectSlice;
use demod_fm::FmDemod;
use map_in_place::MapInPlace;
use mio;
use moving_avg::MovingAverage;
use num::complex::Complex32;
use num::traits::Zero;
use p25_filts::{DecimFIR, BandpassFIR};
use pool::{Pool, Checkout};
use rtlsdr_iq::IQ;
use static_decimate::{Decimator, DecimationFactor};
use static_fir::FIRFilter;
use throttle::Throttler;

use hub::HubEvent;
use recv::RecvEvent;
use consts::{BUF_SAMPLES, BASEBAND_SAMPLE_RATE};

/// Demodulates raw I/Q signal to C4FM baseband.
pub struct DemodTask {
    /// Decimates I/Q signal.
    decim: Decimator<Decimate5, DecimFIR>,
    /// Channel-select lowpass filter.
    bandpass: FIRFilter<BandpassFIR>,
    /// Moving average filter.
    avg: MovingAverage<f32>,
    /// Demodulates FM signal.
    demod: FmDemod,
    /// Channel for receiving I/Q sample chunks.
    reader: Receiver<Checkout<Vec<u8>>>,
    /// Channel for the hub.
    hub: mio::channel::Sender<HubEvent>,
    /// Channel for sending baseband sample chunks.
    chan: Sender<RecvEvent>,
}

impl DemodTask {
    /// Create a new `DemodTask` that receives I/Q sample chunks from the first given
    /// channel, sends events to the second given hub, and sends baseband sample chunks to
    /// the third given channel.
    pub fn new(reader: Receiver<Checkout<Vec<u8>>>,
               hub: mio::channel::Sender<HubEvent>,
               chan: Sender<RecvEvent>)
        -> Self
    {
        DemodTask {
            decim: Decimator::new(),
            bandpass: FIRFilter::new(),
            avg: MovingAverage::new(10),
            // Assume a 5kHz frequency deviation.
            demod: FmDemod::new(5000, BASEBAND_SAMPLE_RATE),
            reader: reader,
            hub: hub,
            chan: chan,
        }
    }

    /// Begin demodulating, blocking the current thread.
    pub fn run(&mut self) {
        let mut pool = Pool::with_capacity(16, || vec![0.0; BUF_SAMPLES]);
        let mut samples = vec![Complex32::zero(); BUF_SAMPLES];

        // Used to reduce the number of signal level messages sent.
        let mut notifier = Throttler::new(16);

        loop {
            let bytes = self.reader.recv().expect("unable to receive sdr samples");

            // This is safe because it's transforming an array of N 8-bit words to an
            // array of N/2 16-bit words.
            let pairs = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const u16, BUF_SAMPLES)
            };

            // This is safe because it equals the original allocation length.
            unsafe { samples.set_len(BUF_SAMPLES); }

            // Transform interleaved byte pairs to complex floating point samples.
            pairs.iter()
                 .map(|&s| unsafe { *IQ.get_unchecked(s as usize) })
                 .collect_slice(&mut samples[..]);

            // Decimate from SDR to baseband sample rate.
            let len = self.decim.decim_in_place(&mut samples[..]);

            // This is safe because the decimated length is less than the original length.
            unsafe { samples.set_len(len); }

            // Apply bandpass filter to attenuate out-of-channel interference.
            samples.map_in_place(|&s| self.bandpass.feed(s));

            notifier.throttle(|| {
                // Calculate power assuming a "normalized" resistance.
                let power = power_dbm(&samples[..], 1.0);

                self.hub.send(HubEvent::UpdateSignalPower(power))
                    .expect("unable to send signal power");
            });

            let mut baseband = pool.checkout().expect("unable to allocate baseband");

            // This is safe because each input sample produces exactly one output sample.
            unsafe { baseband.set_len(samples.len()); }

            // Demodulate FM signal to C4FM baseband.
            samples.iter()
                   .map(|&s| self.demod.feed(s))
                   .collect_slice(&mut baseband[..]);

            // Apply averaging filter.
            baseband.map_in_place(|&s| self.avg.feed(s));

            self.chan.send(RecvEvent::Baseband(baseband))
                .expect("unable to send baseband");
        }
    }
}

/// Decimates from 240kHz to 48kHz.
struct Decimate5;

impl DecimationFactor for Decimate5 {
    fn factor() -> u32 { 5 }
}

/// Calculate the power (dBm) into the resistance (ohms) of the given samples.
pub fn power_dbm(samples: &[Complex32], resistance: f32) -> f32 {
    // Units of Watt-ohms
    let avg = samples.iter().fold(0.0, |s, x| {
        s + x.norm_sqr()
    }) / samples.len() as f32;

    // Power in Watts.
    let power = avg / resistance;

    // Convert Watts to dBm.
    30.0 + 10.0 * power.log10()
}
