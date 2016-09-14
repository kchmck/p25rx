use imbe::decoder::{IMBEDecoder, CAIFrame};
use imbe;
use map_in_place::MapInPlace;
use p25::voice::frame::VoiceFrame;
use std::io::Write;
use std::sync::mpsc::Receiver;
use std;

pub enum AudioEvent {
    VoiceFrame(VoiceFrame),
    EndTransmission,
}

pub struct Audio<W: Write> {
    imbe: IMBEDecoder,
    out: W,
    queue: Receiver<AudioEvent>,
}

impl<W: Write> Audio<W> {
    pub fn new(out: W, queue: Receiver<AudioEvent>) -> Audio<W> {
        Audio {
            imbe: IMBEDecoder::new(),
            out: out,
            queue: queue,
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.queue.recv().expect("unable to receive audio event") {
                AudioEvent::VoiceFrame(vf) => {
                    let frame = CAIFrame::new(vf.chunks, vf.errors);

                    let mut samples = [0.0; imbe::consts::SAMPLES];
                    self.imbe.decode(frame, &mut samples);
                    samples.map_in_place(|&s| s / 8192.0);

                    self.out.write_all(unsafe {
                        std::slice::from_raw_parts(samples.as_ptr() as *const u8,
                            samples.len() * 4)
                    }).unwrap();
                },
                AudioEvent::EndTransmission => self.out.flush().unwrap(),
            }
        }
    }
}
