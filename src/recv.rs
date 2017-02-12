use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::io::{Read, Write};
use std::sync::mpsc::{Sender, Receiver};
use std;

use fnv::FnvHasher;
use p25::message::nid::DataUnit;
use p25::message::receiver::MessageReceiver;
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{TSBKOpcode};
use p25::voice::crypto::CryptoAlgorithm;
use pool::Checkout;

use audio::{AudioEvent, AudioOutput};
use sdr::ControlTaskEvent;
use ui::UIEvent;

pub enum ReceiverEvent {
    Baseband(Checkout<Vec<f32>>),
    SetControlFreq(u32),
}

pub struct RecvTask {
    control_freq: u32,
    msg: MessageReceiver,
    channels: ChannelParamsMap,
    cur_talkgroup: TalkGroup,
    encrypted: HashSet<u16, BuildHasherDefault<FnvHasher>>,
    events: Receiver<ReceiverEvent>,
    ui: Sender<UIEvent>,
    sdr: Sender<ControlTaskEvent>,
    audio: Sender<AudioEvent>,
}

impl RecvTask {
    pub fn new(freq: u32,
               events: Receiver<ReceiverEvent>,
               ui: Sender<UIEvent>,
               sdr: Sender<ControlTaskEvent>,
               audio: Sender<AudioEvent>)
        -> Self
    {
        RecvTask {
            control_freq: freq,
            msg: MessageReceiver::new(),
            channels: ChannelParamsMap::default(),
            cur_talkgroup: TalkGroup::Default,
            encrypted: HashSet::default(),
            events: events,
            ui: ui,
            sdr: sdr,
            audio: audio,
        }.init()
    }

    fn init(self) -> Self {
        self.switch_control();
        self
    }

    fn switch_control(&self) {
        self.set_freq(self.control_freq);
    }

    fn set_freq(&self, freq: u32) {
        self.ui.send(UIEvent::SetFreq(freq))
            .expect("unable to update freq in UI");
        self.sdr.send(ControlTaskEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");
    }

    pub fn run<F>(&mut self, mut cb: F)
        where F: FnMut(&[f32])
    {
        loop {
            match self.events.recv().expect("unable to receive baseband") {
                ReceiverEvent::Baseband(samples) => {
                    for &s in samples.iter() {
                        self.handle_sample(s);
                    }

                    cb(&samples[..]);
                },
                ReceiverEvent::SetControlFreq(freq) => {
                    self.control_freq = freq;
                    self.switch_control();
                },
            }
        }
    }

    fn handle_sample(&mut self, s: f32) {
        use p25::message::receiver::MessageEvent::*;

        let event = match self.msg.feed(s) {
            Some(event) => event,
            None => return,
        };

        match event {
            Error(_) => {},
            PacketNID(nid) => {
                match nid.data_unit {
                    DataUnit::VoiceLCTerminator | DataUnit::VoiceSimpleTerminator => {
                        self.switch_control();
                        self.audio.send(AudioEvent::EndTransmission)
                            .expect("unable to send end of transmission");

                        self.msg.recv.resync();
                    },
                    _ => {},
                }
            },
            VoiceHeader(head) => self.handle_crypto(head.crypto_alg()),
            LinkControl(_) => {},
            CryptoControl(cc) => self.handle_crypto(cc.alg()),
            LowSpeedDataFragment(_) => {},
            VoiceFrame(vf) => {
                self.audio.send(AudioEvent::VoiceFrame(vf))
                    .expect("unable to send voice frame");
            },
            TrunkingControl(tsbk) => {
                if tsbk.mfg() != 0 {
                    return;
                }

                if !tsbk.crc_valid() {
                    return;
                }

                let opcode = match tsbk.opcode() {
                    Some(o) => o,
                    None => return,
                };

                match opcode {
                    TSBKOpcode::GroupVoiceUpdate => {
                        let updates = fields::GroupTrafficUpdate::new(tsbk.payload())
                                          .updates();

                        for (ch, tg) in updates.iter().cloned() {
                            if self.use_talkgroup(tg, ch) {
                                self.msg.recv.resync();
                                break;
                            }
                        }
                    },
                    TSBKOpcode::ChannelParamsUpdate => {
                        let dec = fields::ChannelParamsUpdate::new(tsbk.payload());
                        self.channels[dec.id() as usize] = Some(dec.params());
                    },
                    _ => {},
                }
            }
            VoiceTerm(_) => {},
        }
    }

    fn handle_crypto(&mut self, alg: CryptoAlgorithm) {
        if let CryptoAlgorithm::Unencrypted = alg {
            return;
        }

        self.switch_control();
        self.msg.recv.resync();

        if let TalkGroup::Other(x) = self.cur_talkgroup {
            self.encrypted.insert(x);
        }
    }

    fn use_talkgroup(&mut self, tg: TalkGroup, ch: Channel) -> bool {
        if let TalkGroup::Other(x) = tg {
            if self.encrypted.contains(&x) {
                return false;
            }
        }

        let freq = match self.channels[ch.id() as usize] {
            Some(p) => p.rx_freq(ch.number()),
            None => return false,
        };

        self.cur_talkgroup = tg;

        self.set_freq(freq);
        self.ui.send(UIEvent::SetTalkGroup(tg)).expect("unable to send talkgroup");

        true
    }
}

pub struct ReplayReceiver<W: Write> {
    audio: AudioOutput<W>,
    msg: MessageReceiver,
}

impl<W: Write> ReplayReceiver<W> {
    pub fn new(audio: AudioOutput<W>) -> Self {
        ReplayReceiver {
            audio: audio,
            msg: MessageReceiver::new(),
        }
    }

    pub fn replay<R: Read>(&mut self, stream: &mut R) {
        let mut buf = [0; 32768];

        loop {
            let size = stream.read(&mut buf).expect("unable to read samples");

            if size == 0 {
                break;
            }

            let samples: &[f32] = unsafe {
                std::slice::from_raw_parts(
                    buf.as_ptr() as *const f32,
                    size / std::mem::size_of::<f32>()
                )
            };

            self.feed(samples);
        }
    }

    fn feed(&mut self, samples: &[f32]) {
        use p25::message::receiver::MessageEvent::*;

        for &sample in samples {
            let event = match self.msg.feed(sample) {
                Some(event) => event,
                None => continue,
            };

            match event {
                VoiceFrame(vf) => self.audio.play(&vf),
                _ => {},
            }
        }
    }
}
