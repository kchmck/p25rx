use p25::error::P25Error;
use p25::message::{MessageReceiver, MessageHandler};
use p25::nid::DataUnit;
use p25::nid::NetworkID;
use p25::receiver::DataUnitReceiver;
use p25::trunking::decode::TalkGroup;
use p25::trunking::tsbk::{self, TSBKFields, TSBKOpcode};
use p25::voice::control::LinkControlFields;
use p25::voice::crypto::CryptoControlFields;
use p25::voice::frame::VoiceFrame;
use p25::voice::header::VoiceHeaderFields;
use pi25_cfg::sites::P25Sites;
use pool::Checkout;
use std::sync::Arc;
use std::sync::mpsc::{Sender, Receiver};
use std;

use audio::AudioEvent;
use sdr::ControllerEvent;
use ui::UIEvent;

pub enum ReceiverEvent {
    Baseband(Checkout<Vec<f32>>),
    SetSite(usize),
    /// Channel frequency has been switched.
    ChannelUpdated,
}

/// State of the receiver.
#[derive(Copy, Clone)]
enum State {
    /// Receiving and decoding normally.
    Receive,
    /// Waiting until the channel frequency has been switched.
    WaitChannel,
}

pub struct P25Receiver {
    sites: Arc<P25Sites>,
    site: usize,
    state: State,
    events: Receiver<ReceiverEvent>,
    ui: Sender<UIEvent>,
    sdr: Sender<ControllerEvent>,
    audio: Sender<AudioEvent>,
}

impl P25Receiver {
    pub fn new(sites: Arc<P25Sites>,
               events: Receiver<ReceiverEvent>,
               ui: Sender<UIEvent>,
               sdr: Sender<ControllerEvent>,
               audio: Sender<AudioEvent>)
        -> P25Receiver
    {
        P25Receiver {
            sites: sites,
            events: events,
            site: std::usize::MAX,
            state: State::WaitChannel,
            ui: ui,
            sdr: sdr,
            audio: audio,
        }
    }

    fn switch_control(&mut self) {
        let site = self.sites[self.site].control;
        self.set_freq(site);
    }

    fn set_freq(&mut self, freq: u32) {
        self.state = State::WaitChannel;

        self.ui.send(UIEvent::SetFreq(freq))
            .expect("unable to update freq in UI");
        self.sdr.send(ControllerEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");
    }

    pub fn run(&mut self) {
        let mut messages = MessageReceiver::new();

        loop {
            match self.events.recv().expect("unable to receive baseband") {
                ReceiverEvent::Baseband(samples) => match self.state {
                    State::Receive => {
                        for &s in samples.iter() {
                            messages.feed(s, self);
                        }
                    },
                    // Consider baseband samples invalid until channel has been switched.
                    State::WaitChannel => {},
                },
                ReceiverEvent::SetSite(site) => {
                    self.site = site;
                    self.switch_control();
                },
                ReceiverEvent::ChannelUpdated => match self.state {
                    State::WaitChannel => self.state = State::Receive,
                    _ => panic!("unexpected channel update"),
                },
            }
        }
    }
}

impl MessageHandler for P25Receiver {
    fn handle_error(&mut self, _: &mut DataUnitReceiver, _: P25Error) {}

    fn handle_nid(&mut self, recv: &mut DataUnitReceiver, nid: NetworkID) {
        match nid.data_unit() {
            DataUnit::VoiceLCTerminator | DataUnit::VoiceSimpleTerminator => {
                self.switch_control();
                self.audio.send(AudioEvent::EndTransmission)
                    .expect("unable to send end of transmission");

                recv.resync();
            },
            _ => {},
        }
    }

    fn handle_header(&mut self, _: &mut DataUnitReceiver, _: VoiceHeaderFields) {}
    fn handle_lc(&mut self, _: &mut DataUnitReceiver, _: LinkControlFields) {}
    fn handle_cc(&mut self, _: &mut DataUnitReceiver, _: CryptoControlFields) {}
    fn handle_data_frag(&mut self, _: &mut DataUnitReceiver, _: u32) {}

    fn handle_frame(&mut self, _: &mut DataUnitReceiver, vf: VoiceFrame) {
        self.audio.send(AudioEvent::VoiceFrame(vf))
            .expect("unable to send voice frame");
    }

    fn handle_tsbk(&mut self, _: &mut DataUnitReceiver, tsbk: TSBKFields) {
        if tsbk.mfg() != 0 {
            return;
        }
        if tsbk.crc() != tsbk.calc_crc() {
            return;
        }

        let opcode = match tsbk.opcode() {
            Some(o) => o,
            None => return,
        };

        match opcode {
            TSBKOpcode::GroupVoiceUpdate => {
                let dec = tsbk::GroupVoiceUpdate::new(tsbk);
                let ch1 = dec.channel_a();
                let ch2 = dec.channel_b();

                if dec.talk_group_a() == TalkGroup::Other(0xCB68) {
                    return;
                }

                let freq = match self.sites[self.site].traffic.get(&ch1.number()) {
                    Some(&freq) => freq,
                    None => {
                        println!("talkgroup 1:{:?}", dec.talk_group_a());
                        println!("  number:{}", ch1.number());
                        println!("talkgroup 2:{:?}", dec.talk_group_b());
                        println!("  number:{}", ch2.number());

                        return;
                    },
                };

                self.set_freq(freq);
                self.ui.send(UIEvent::SetTalkGroup(dec.talk_group_a()))
                    .expect("unable to send talkgroup");
            },
            _ => {},
        }
    }

    fn handle_term(&mut self, _: &mut DataUnitReceiver) {}
}
