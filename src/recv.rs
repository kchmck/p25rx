use fnv::FnvHasher;
use p25::message::nid::DataUnit;
use p25::message::receiver::{MessageReceiver};
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{TSBKOpcode};
use p25::voice::crypto::CryptoAlgorithm;
use pool::Checkout;
use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::io::Write;
use std::sync::mpsc::{Sender, Receiver};
use std;

use audio::AudioEvent;
use sdr::ControllerEvent;
use ui::UIEvent;

pub enum ReceiverEvent {
    Baseband(Checkout<Vec<f32>>),
    SetControlFreq(u32),
}

pub struct P25Receiver<W: Write> {
    control_freq: u32,
    samples_stream: Option<W>,
    msg: MessageReceiver,
    channels: ChannelParamsMap,
    cur_talkgroup: TalkGroup,
    encrypted: HashSet<u16, BuildHasherDefault<FnvHasher>>,
    events: Receiver<ReceiverEvent>,
    ui: Sender<UIEvent>,
    sdr: Sender<ControllerEvent>,
    audio: Sender<AudioEvent>,
}

impl<W: Write> P25Receiver<W> {
    pub fn new(samples_stream: Option<W>,
               events: Receiver<ReceiverEvent>,
               ui: Sender<UIEvent>,
               sdr: Sender<ControllerEvent>,
               audio: Sender<AudioEvent>)
        -> Self
    {
        P25Receiver {
            control_freq: 0,
            samples_stream: samples_stream,
            msg: MessageReceiver::new(),
            channels: ChannelParamsMap::default(),
            cur_talkgroup: TalkGroup::Default,
            encrypted: HashSet::default(),
            events: events,
            ui: ui,
            sdr: sdr,
            audio: audio,
        }
    }

    fn switch_control(&self) {
        self.set_freq(self.control_freq);
    }

    fn set_freq(&self, freq: u32) {
        self.ui.send(UIEvent::SetFreq(freq))
            .expect("unable to update freq in UI");
        self.sdr.send(ControllerEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");
    }

    pub fn run(&mut self) {
        loop {
            match self.events.recv().expect("unable to receive baseband") {
                ReceiverEvent::Baseband(samples) => {
                    for &s in samples.iter() {
                        self.handle_sample(s);
                    }

                    self.write_samples(&samples[..]);
                },
                ReceiverEvent::SetControlFreq(freq) => self.control_freq = freq,
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

    fn write_samples(&mut self, samples: &[f32]) {
        if let Some(ref mut stream) = self.samples_stream {
            stream.write_all(unsafe {
                std::slice::from_raw_parts(
                    samples.as_ptr() as *const u8,
                    samples.len() * std::mem::size_of::<f32>()
                )
            }).expect("unable to write samples");
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
