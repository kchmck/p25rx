use std::sync::mpsc::{Sender, Receiver};

use pool::{Pool, Checkout};
use rtlsdr::{Controller, Reader};

use consts::{BUF_SIZE_RAW, BUF_COUNT};

pub struct BlockReader {
    chan: Sender<Checkout<Vec<u8>>>,
}

impl BlockReader {
    pub fn new(chan: Sender<Checkout<Vec<u8>>>) -> BlockReader {
        BlockReader {
            chan: chan,
        }
    }

    pub fn run(&mut self, mut reader: Reader) {
        let mut pool = Pool::with_capacity(16, || vec![0; BUF_SIZE_RAW]);

        reader.read_async(BUF_COUNT as u32, BUF_SIZE_RAW as u32, |bytes| {
            let mut samples = pool.checkout().expect("unable to allocate samples");
            (&mut samples[..]).copy_from_slice(bytes);
            self.chan.send(samples).expect("unable to send sdr samples");
        }).expect("error in async read");
    }
}

pub enum ControlTaskEvent {
    SetFreq(u32),
}

pub struct ControlTask {
    sdr: Controller,
    events: Receiver<ControlTaskEvent>,
}

impl ControlTask {
    pub fn new(sdr: Controller, events: Receiver<ControlTaskEvent>) -> ControlTask {
        ControlTask {
            sdr: sdr,
            events: events,
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.events.recv().expect("unable to receive controller event") {
                ControlTaskEvent::SetFreq(freq) => {
                    println!("SetFreq({})", freq);
                    self.sdr.set_center_freq(freq).expect("unable to set frequency");
                },
            }
        }
    }
}
