//! Voice frame decoding and audio output.

use std::io::Write;
use std::sync::mpsc::Receiver;

use imbe::consts::SAMPLES_PER_FRAME;
use imbe::decode::ImbeDecoder;
use imbe::frame::ReceivedFrame;
use map_in_place::MapInPlace;
use p25::voice::frame::VoiceFrame;
use slice_cast;

/// Messages for `AudioTask`.
pub enum AudioEvent {
    /// A voice frame was received.
    VoiceFrame(VoiceFrame),
    /// The current voice transmission has been terminated.
    EndTransmission,
}

/// Decodes voice frames and outputs them to a stream.
pub struct AudioTask<W: Write> {
    /// Decodes and outputs frames.
    audio: AudioOutput<W>,
    /// Channel for messages.
    events: Receiver<AudioEvent>,
}

impl<W: Write> AudioTask<W> {
    /// Create a new `AudioTask` with the given audio output and event channel.
    pub fn new(audio: AudioOutput<W>, events: Receiver<AudioEvent>) -> Self {
        AudioTask {
            audio: audio,
            events: events,
        }
    }

    /// Begin handling events, blocking the current thread.
    pub fn run(&mut self) {
        loop {
            match self.events.recv().expect("unable to receive audio event") {
                AudioEvent::VoiceFrame(vf) => self.audio.play(&vf),
                AudioEvent::EndTransmission => {
                   self.audio.flush();
                   self.audio.reset();
                },
            }
        }
    }
}

/// Outputs voice frames to a stream.
pub struct AudioOutput<W: Write> {
    /// Stream to write to.
    stream: W,
    /// Voice frame decoder.
    imbe: ImbeDecoder,
}

impl<W: Write> AudioOutput<W> {
    /// Create a new `AudioOutput` over the given stream.
    pub fn new(stream: W) -> Self {
        AudioOutput {
            stream: stream,
            imbe: ImbeDecoder::new(),
        }
    }

    /// Reinitialize the voice decoder for a new transmission.
    pub fn reset(&mut self) {
        self.imbe = ImbeDecoder::new();
    }

    /// Decode and output the given frame.
    pub fn play(&mut self, frame: &VoiceFrame) {
        let frame = ReceivedFrame::new(frame.chunks, frame.errors);

        let mut samples = [0.0; SAMPLES_PER_FRAME];
        self.imbe.decode(frame, &mut samples);

        // Reduce volume to a generally sane level.
        samples.map_in_place(|&s| s / 8192.0);

        self.stream.write_all(unsafe {
            slice_cast::cast(&samples[..])
        }).expect("unable to write audio samples");
    }

    /// Flush the wrapped stream.
    pub fn flush(&mut self) {
        self.stream.flush().expect("unable to flush audio samples")
    }
}
