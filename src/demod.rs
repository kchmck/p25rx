use collect_slice::CollectSlice;
use static_decimate::{Decimator, DecimationFactor};
use static_fir::FIRFilter;
use demod_fm::FMDemod;
use map_in_place::MapInPlace;
use num::complex::Complex32;
use num::traits::Zero;
use pool::{Pool, Checkout};
use rtlsdr_iq::IQ;
use sigpower::power;
use sigpower::smeter::SignalLevel;
use std::sync::mpsc::{Sender, Receiver};
use std;
use throttle::Throttler;

use p25_filts::{DecimFIR, BandpassFIR};
use ui::UIEvent;
use recv::ReceiverEvent;
use consts::{BUF_SIZE_COMPLEX, BASEBAND_SAMPLE_RATE};

const FM_DEV: u32 = 5000;
const POWER_ADJUST: f32 = -106.0;

pub struct Demod {
    decim: Decimator<Decimate5, DecimFIR>,
    bandpass: FIRFilter<BandpassFIR>,
    demod: FMDemod,
    reader: Receiver<Checkout<Vec<u8>>>,
    ui: Sender<UIEvent>,
    chan: Sender<ReceiverEvent>,
}

impl Demod {
    pub fn new(reader: Receiver<Checkout<Vec<u8>>>,
               ui: Sender<UIEvent>,
               chan: Sender<ReceiverEvent>)
        -> Demod
    {
        Demod {
            decim: Decimator::new(),
            bandpass: FIRFilter::new(),
            demod: FMDemod::new(FM_DEV as f32 / BASEBAND_SAMPLE_RATE as f32),
            reader: reader,
            ui: ui,
            chan: chan,
        }
    }

    pub fn run(&mut self) {
        let mut pool = Pool::with_capacity(16, || vec![0.0; BUF_SIZE_COMPLEX]);
        let mut samples = vec![Complex32::zero(); BUF_SIZE_COMPLEX];

        let mut notifier = Throttler::new(16);

        loop {
            let bytes = self.reader.recv().expect("unable to receive sdr samples");

            let pairs = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const u16, BUF_SIZE_COMPLEX)
            };

            unsafe { samples.set_len(BUF_SIZE_COMPLEX); }

            pairs.iter()
                 .map(|&s| unsafe { *IQ.get_unchecked(s as usize) })
                 .collect_slice(&mut samples[..]);

            let len = self.decim.decim_in_place(&mut samples[..]);

            // This is safe for the same reason as above.
            unsafe { samples.set_len(len); }

            samples.map_in_place(|&s| self.bandpass.feed(s));

            let level = SignalLevel::from_dbm(
                power::power_dbm(&samples[..], 50.0) + POWER_ADJUST);

            notifier.throttle(|| {
                self.ui.send(UIEvent::SetSignalLevel(level))
                    .expect("unable to send signal level");
            });

            let mut baseband = pool.checkout().expect("unable to allocate baseband");

            // This is safe because each input sample produces exactly one output sample.
            unsafe { baseband.set_len(samples.len()); }

            samples.iter()
                   .map(|&s| self.demod.feed(s))
                   .collect_slice(&mut baseband[..]);

            self.chan.send(ReceiverEvent::Baseband(baseband))
                .expect("unable to send baseband");
        }
    }
}

struct Decimate5;

impl DecimationFactor for Decimate5 {
    fn factor() -> u32 { 5 }
}
