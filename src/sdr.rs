//! Interface to RTL-SDR.

use std::sync::mpsc::{Sender, Receiver};

use pool::{Pool, Checkout};
use rtlsdr::{Controller, Reader};

use consts::{BUF_BYTES, BUF_COUNT};

/// Reads chunks of samples from the SDR and sends them over a channel.
pub struct ReadTask {
    /// Channel to send chunks over.
    chan: Sender<Checkout<Vec<u8>>>,
}

impl ReadTask {
    /// Create a new `ReadTask` communicating over the given channel.
    pub fn new(chan: Sender<Checkout<Vec<u8>>>) -> Self {
        ReadTask {
            chan: chan,
        }
    }

    /// Start reading samples, blocking the thread.
    pub fn run(&mut self, mut reader: Reader) {
        let mut pool = Pool::with_capacity(16, || vec![0; BUF_BYTES]);

        reader.read_async(BUF_COUNT as u32, BUF_BYTES as u32, |bytes| {
            let mut samples = pool.checkout().expect("unable to allocate samples");
            (&mut samples[..]).copy_from_slice(bytes);
            self.chan.send(samples).expect("unable to send sdr samples");
        }).expect("error in async read");
    }
}

/// Messages for `ControlTask`.
pub enum ControlTaskEvent {
    /// Set the center frequency to the contained value (Hz).
    SetFreq(u32),
}

/// Controls SDR parameters.
pub struct ControlTask {
    /// SDR interface.
    sdr: Controller,
    /// Channel for messages.
    events: Receiver<ControlTaskEvent>,
}

impl ControlTask {
    /// Create a new `ControlTask` over the given SDR, receiving messages from the given
    /// channel.
    pub fn new(sdr: Controller, events: Receiver<ControlTaskEvent>) -> Self {
        ControlTask {
            sdr: sdr,
            events: events,
        }
    }

    /// Start managing the SDR, blocking the thread.
    pub fn run(&mut self) {
        loop {
            match self.events.recv().expect("unable to receive controller event") {
                ControlTaskEvent::SetFreq(freq) =>
                    self.sdr.set_center_freq(freq).expect("unable to set frequency"),
            }
        }
    }
}
