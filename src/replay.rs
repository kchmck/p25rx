//! Replay saved baseband recordings.

use std::io::{Read, Write};

use p25::message::receiver::MessageReceiver;
use p25::stats::Stats;
use slice_cast;

use audio::AudioOutput;

pub struct ReplayReceiver<W: Write> {
    audio: AudioOutput<W>,
    msg: MessageReceiver,
    stats: Stats,
}

impl<W: Write> ReplayReceiver<W> {
    pub fn new(audio: AudioOutput<W>) -> Self {
        ReplayReceiver {
            audio: audio,
            msg: MessageReceiver::new(),
            stats: Stats::default(),
        }
    }

    pub fn replay<R: Read>(&mut self, stream: &mut R) {
        let mut buf = [0; 32768];

        loop {
            let size = stream.read(&mut buf).expect("unable to read samples");

            if size == 0 {
                break;
            }

            self.feed(unsafe { slice_cast::cast(&buf[..]) });
        }
    }

    fn feed(&mut self, samples: &[f32]) {
        use p25::message::receiver::MessageEvent::*;

        for &sample in samples {
            let event = match self.msg.feed(sample) {
                Some(event) => event,
                None => continue,
            };

            self.stats.merge(&mut self.msg);

            match event {
                Error(e) => self.stats.record_err(e),
                VoiceFrame(vf) => self.audio.play(&vf),
                _ => {},
            }
        }
    }
}
