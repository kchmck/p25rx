use p25::error::P25Error;
use p25::message::{MessageReceiver, MessageHandler};
use p25::nid::NetworkID;
use p25::trunking::decode::TalkGroup;
use p25::trunking::tsbk::{self, TSBKFields, TSBKOpcode};
use p25::voice::control::LinkControlFields;
use p25::voice::crypto::CryptoControlFields;
use p25::voice::frame::VoiceFrame;
use p25::voice::header::VoiceHeaderFields;
use p25_filts::DeemphFIR;
use pi25_cfg::sites::P25Sites;
use pool::Checkout;
use static_fir::FIRFilter;
use std::sync::Arc;
use std::sync::mpsc::{Sender, Receiver};

use audio::AudioEvent;
use consts::DEFAULT_SITE;
use sdr::ControllerEvent;
use ui::UIEvent;

pub enum ReceiverEvent {
    Baseband(Checkout<Vec<f32>>),
    SetSite(usize),
}

pub struct P25Receiver {
    deemph: FIRFilter<DeemphFIR>,
    sites: Arc<P25Sites>,
    site: usize,
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
            deemph: FIRFilter::new(),
            sites: sites,
            events: events,
            site: DEFAULT_SITE,
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
        self.set_freq(self.sites[self.site].control);
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
                        messages.feed(self.deemph.feed(s), self);
                    }
                },
                ReceiverEvent::SetSite(site) => {
                    self.site = site;
                    self.switch_control();
                },
            }
        }
    }
}

impl MessageHandler for P25Receiver {
    fn handle_error(&mut self, _: P25Error) {}
    fn handle_nid(&mut self, _: NetworkID) {}
    fn handle_header(&mut self, _: VoiceHeaderFields) {}
    fn handle_lc(&mut self, _: LinkControlFields) {}
    fn handle_cc(&mut self, _: CryptoControlFields) {}
    fn handle_data_frag(&mut self, _: u32) {}

    fn handle_frame(&mut self, vf: VoiceFrame) {
        self.audio.send(AudioEvent::VoiceFrame(vf))
            .expect("unable to send voice frame");
    }

    fn handle_tsbk(&mut self, tsbk: TSBKFields) {
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

    fn handle_term(&mut self) {
        self.switch_control();
        self.audio.send(AudioEvent::EndTransmission)
            .expect("unable to send end of transmission");
    }
}


