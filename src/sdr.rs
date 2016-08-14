use std::sync::mpsc::{Sender, Receiver};
use pool::{Pool, Checkout};
use rtlsdr::{Control, Reader};
use std::io::Write;

use consts::{BUF_SIZE, BUF_COUNT};

pub struct Radio {
    chan: Sender<Checkout<Vec<u8>>>,
}

impl Radio {
    pub fn new(chan: Sender<Checkout<Vec<u8>>>) -> Radio {
        Radio {
            chan: chan,
        }
    }

    pub fn run(&mut self, mut reader: Reader) {
        let mut pool = Pool::with_capacity(16, || vec![0; BUF_SIZE]);

        reader.read_async(BUF_COUNT as u32, BUF_SIZE as u32, |bytes| {
            let mut samples = pool.checkout().expect("unable to allocate samples");
            (&mut samples[..]).write(bytes).unwrap();
            self.chan.send(samples).expect("unable to send sdr samples");
        });
    }
}

pub enum ControllerEvent {
    SetFreq(u32),
    SetGain(i32),
}

pub struct Controller {
    sdr: Control,
    events: Receiver<ControllerEvent>,
}

impl Controller {
    pub fn new(sdr: Control, events: Receiver<ControllerEvent>) -> Controller {
        Controller {
            sdr: sdr,
            events: events,
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.events.recv().expect("unable to receive controller event") {
                ControllerEvent::SetFreq(freq) =>
                    assert!(self.sdr.set_center_freq(freq)),
                ControllerEvent::SetGain(gain) =>
                    assert!(self.sdr.set_tuner_gain(gain)),
            }
        }
    }
}
