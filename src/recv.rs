use fnv::FnvHasher;
use p25::error::P25Error;
use p25::message::data_unit::DataUnitReceiver;
use p25::message::nid::{DataUnit, NetworkID};
use p25::message::receiver::{MessageReceiver, MessageHandler};
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{TSBKFields, TSBKOpcode};
use p25::voice::control::{LinkControlFields, LinkControlOpcode};
use p25::voice::crypto::{CryptoAlgorithm, CryptoControlFields};
use p25::voice::frame::VoiceFrame;
use p25::voice::header::VoiceHeaderFields;
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
            events: events,
            channels: ChannelParamsMap::default(),
            cur_talkgroup: TalkGroup::Default,
            encrypted: HashSet::default(),
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
        let mut messages = MessageReceiver::new();

        loop {
            match self.events.recv().expect("unable to receive baseband") {
                ReceiverEvent::Baseband(samples) => {
                    for &s in samples.iter() {
                        messages.feed(s, self);
                    }

                    self.write_samples(&samples[..]);
                },
                ReceiverEvent::SetControlFreq(freq) => self.control_freq = freq,
            }
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

    fn handle_crypto(&mut self, recv: &mut DataUnitReceiver, alg: CryptoAlgorithm) {
        if let CryptoAlgorithm::Unencrypted = alg {
            return;
        }

        self.switch_control();
        recv.resync();

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

impl<W: Write> MessageHandler for P25Receiver<W> {
    fn handle_error(&mut self, _: &mut DataUnitReceiver, _: P25Error) {}

    fn handle_nid(&mut self, recv: &mut DataUnitReceiver, nid: NetworkID) {
        match nid.data_unit {
            DataUnit::VoiceLCTerminator | DataUnit::VoiceSimpleTerminator => {
                self.switch_control();
                self.audio.send(AudioEvent::EndTransmission)
                    .expect("unable to send end of transmission");

                recv.resync();
            },
            _ => {},
        }
    }

    fn handle_header(&mut self, recv: &mut DataUnitReceiver, head: VoiceHeaderFields) {
        self.handle_crypto(recv, head.crypto_alg());
    }

    fn handle_lc(&mut self, _: &mut DataUnitReceiver, _: LinkControlFields) {}

    fn handle_cc(&mut self, recv: &mut DataUnitReceiver, cc: CryptoControlFields) {
        self.handle_crypto(recv, cc.alg());
    }

    fn handle_data_frag(&mut self, _: &mut DataUnitReceiver, _: u32) {}

    fn handle_frame(&mut self, _: &mut DataUnitReceiver, vf: VoiceFrame) {
        self.audio.send(AudioEvent::VoiceFrame(vf))
            .expect("unable to send voice frame");
    }

    fn handle_tsbk(&mut self, recv: &mut DataUnitReceiver, tsbk: TSBKFields) {
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
                let updates = fields::GroupTrafficUpdate::new(tsbk.payload()).updates();

                for (ch, tg) in updates.iter().cloned() {
                    if self.use_talkgroup(tg, ch) {
                        recv.resync();
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

    fn handle_term(&mut self, _: &mut DataUnitReceiver) {}
}
