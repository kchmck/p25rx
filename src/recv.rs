use std::collections::HashSet;
use std::hash::BuildHasherDefault;
use std::io::{Read, Write};
use std::sync::mpsc::{Sender, Receiver};
use std;

use fnv::FnvHasher;
use mio_more;
use p25::message::nid::DataUnit;
use p25::message::receiver::MessageReceiver;
use p25::stats::Stats;
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{TsbkOpcode};
use p25::voice::control::{self, LinkControlFields};
use p25::voice::crypto::CryptoAlgorithm;
use pool::Checkout;
use slice_cast;

use audio::{AudioEvent, AudioOutput};
use sdr::ControlTaskEvent;
use hub::{HubEvent, StateEvent};

pub enum RecvEvent {
    Baseband(Checkout<Vec<f32>>),
    SetControlFreq(u32),
}

pub struct RecvTask {
    ctlfreq: u32,
    curfreq: u32,
    msg: MessageReceiver,
    hopping: bool,
    channels: ChannelParamsMap,
    curgroup: TalkGroup,
    encrypted: HashSet<u16, BuildHasherDefault<FnvHasher>>,
    events: Receiver<RecvEvent>,
    hub: mio_more::channel::Sender<HubEvent>,
    sdr: Sender<ControlTaskEvent>,
    audio: Sender<AudioEvent>,
    stats: Stats,
}

impl RecvTask {
    pub fn new(freq: u32,
               events: Receiver<RecvEvent>,
               hub: mio_more::channel::Sender<HubEvent>,
               sdr: Sender<ControlTaskEvent>,
               audio: Sender<AudioEvent>,
               hopping: bool)
        -> Self
    {
        RecvTask {
            ctlfreq: std::u32::MAX,
            curfreq: std::u32::MAX,
            msg: MessageReceiver::new(),
            hopping: hopping,
            channels: ChannelParamsMap::default(),
            curgroup: TalkGroup::Default,
            encrypted: HashSet::default(),
            events: events,
            hub: hub,
            sdr: sdr,
            audio: audio,
            stats: Stats::default(),
        }.init(freq)
    }

    fn init(mut self, freq: u32) -> Self {
        self.set_control_freq(freq);
        self
    }

    fn set_control_freq(&mut self, freq: u32) {
        // Reinitialize channel parameters if moving to a different channel.
        if freq != self.ctlfreq {
            self.channels = ChannelParamsMap::default();
        }

        self.ctlfreq = freq;

        self.hub.send(HubEvent::State(StateEvent::UpdateCtlFreq(freq)))
            .expect("unable to send control frequency");
        self.switch_control();
    }

    fn switch_control(&mut self) {
        self.audio.send(AudioEvent::EndTransmission)
            .expect("unable to send end of transmission");

        // FIXME: non-lexical borrowing
        let freq = self.ctlfreq;
        self.set_freq(freq);
    }

    fn set_freq(&mut self, freq: u32) {
        self.curfreq = freq;

        self.hub.send(HubEvent::UpdateCurFreq(freq))
            .expect("unable to send current frequency");
        self.sdr.send(ControlTaskEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");

        self.msg.resync();
    }

    pub fn run<F: FnMut(&[f32])>(&mut self, mut cb: F) {
        loop {
            match self.events.recv().expect("unable to receive baseband") {
                RecvEvent::Baseband(samples) => {
                    for &s in samples.iter() {
                        self.handle_sample(s);
                    }

                    cb(&samples[..]);
                },
                RecvEvent::SetControlFreq(freq) => self.set_control_freq(freq),
            }
        }
    }

    fn handle_sample(&mut self, s: f32) {
        use p25::message::receiver::MessageEvent::*;

        let event = match self.msg.feed(s) {
            Some(event) => event,
            None => return,
        };

        self.stats.merge(&mut self.msg);

        match event {
            Error(e) => self.stats.record_err(e),
            PacketNID(nid) => {
                match nid.data_unit {
                    DataUnit::VoiceLCTerminator | DataUnit::VoiceSimpleTerminator =>
                        self.switch_control(),
                    _ => {},
                }
            },
            VoiceHeader(head) => self.handle_crypto(head.crypto_alg()),
            LinkControl(lc) => self.handle_lc(lc),
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

                self.hub.send(HubEvent::TrunkingControl(tsbk))
                    .expect("unable to send trunking control");

                match opcode {
                    TsbkOpcode::GroupVoiceUpdate => {
                        let updates = fields::GroupTrafficUpdate::new(tsbk.payload())
                                          .updates();

                        for (ch, tg) in updates.iter().cloned() {
                            if self.use_talkgroup(tg, ch) {
                                break;
                            }
                        }
                    },
                    TsbkOpcode::ChannelParamsUpdate => {
                        let dec = fields::ChannelParamsUpdate::new(tsbk.payload());
                        self.channels.update(&dec);
                        self.hub.send(HubEvent::State(
                            StateEvent::UpdateChannelParams(tsbk)
                        )).expect("unable to send channel update");
                    },
                    _ => {},
                }
            },
            VoiceTerm(lc) => self.handle_lc(lc),
        }
    }

    fn handle_lc(&mut self, lc: LinkControlFields) {
        use p25::voice::control::LinkControlOpcode;

        let opcode = match lc.opcode() {
            Some(o) => o,
            None => return,
        };

        self.hub.send(HubEvent::LinkControl(lc))
            .expect("unable to send link control");

        match opcode {
            LinkControlOpcode::CallTermination => {
                // TODO: record that this conversation is complete
            },
            LinkControlOpcode::GroupVoiceUpdate => {
                let updates = fields::GroupTrafficUpdate::new(lc.payload()).updates();

                for (ch, tg) in updates.iter().cloned() {
                    // TODO: add talkgroups to list of candidates.
                }
            },
            _ => {},
        }
    }

    fn handle_crypto(&mut self, alg: CryptoAlgorithm) {
        if let CryptoAlgorithm::Unencrypted = alg {
            return;
        }

        self.switch_control();

        if let TalkGroup::Other(x) = self.curgroup {
            self.encrypted.insert(x);
        }
    }

    fn use_talkgroup(&mut self, tg: TalkGroup, ch: Channel) -> bool {
        if let TalkGroup::Other(x) = tg {
            if self.encrypted.contains(&x) {
                return false;
            }
        }

        let freq = match self.channels.lookup(ch.id()) {
            Some(p) => p.rx_freq(ch.number()),
            None => return false,
        };

        self.curgroup = tg;

        if self.hopping {
            self.set_freq(freq);
        }

        self.hub.send(HubEvent::UpdateTalkGroup(tg))
            .expect("unable to send talkgroup");

        true
    }
}

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
