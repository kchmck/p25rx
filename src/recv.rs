//! Receiver logic.

use std::io::{Read, Write};
use std::sync::mpsc::{Sender, Receiver};
use std;

use mio_more;
use p25::message::receiver::MessageReceiver;
use p25::stats::Stats;
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{self, TsbkOpcode};
use p25::voice::control::LinkControlFields;
use p25::voice::crypto::CryptoAlgorithm;
use pool::Checkout;
use slice_cast;

use audio::{AudioEvent, AudioOutput};
use hub::{HubEvent, StateEvent};
use policy::{ReceiverPolicy, PolicyEvent};
use sdr::ControlTaskEvent;
use talkgroups::TalkgroupSelection;

pub enum RecvEvent {
    Baseband(Checkout<Vec<f32>>),
    SetControlFreq(u32),
}

pub struct RecvTask {
    ctlfreq: u32,
    curfreq: u32,
    msg: MessageReceiver,
    hopping: bool,
    policy: ReceiverPolicy,
    talkgroups: TalkgroupSelection,
    channels: ChannelParamsMap,
    curgroup: TalkGroup,
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
               hopping: bool,
               policy: ReceiverPolicy,
               talkgroups: TalkgroupSelection)
        -> Self
    {
        RecvTask {
            ctlfreq: std::u32::MAX,
            curfreq: std::u32::MAX,
            msg: MessageReceiver::new(),
            hopping: hopping,
            policy: policy,
            talkgroups: talkgroups,
            channels: ChannelParamsMap::default(),
            curgroup: TalkGroup::Default,
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
            self.talkgroups.clear_state();
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

        self.policy.enter_control();
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
                    self.talkgroups.record_elapsed(samples.len());

                    for &s in samples.iter() {
                        self.handle_sample(s);
                    }

                    cb(&samples[..]);

                    // FIXME: non-lexical borrowing
                    let event = self.policy.handle_elapsed(samples.len());
                    self.handle_policy(event);
                },
                RecvEvent::SetControlFreq(freq) => self.set_control_freq(freq),
            }
        }
    }

    fn handle_policy(&mut self, e: Option<PolicyEvent>) {
        use self::PolicyEvent::*;

        e.map(|e| match e {
            Resync => self.msg.resync(),
            ReturnControl => self.switch_control(),
            ChooseTalkgroup => {
                if let Some((tg, freq)) = self.talkgroups.select_idle() {
                    self.select_talkgroup(tg, freq);
                }
            },
        });
    }

    fn select_talkgroup(&mut self, tg: u16, freq: u32) {
        if !self.hopping {
            return;
        }

        self.curgroup = TalkGroup::Other(tg);
        self.set_freq(freq);
        self.policy.enter_traffic();

        self.hub.send(HubEvent::UpdateTalkGroup(TalkGroup::Other(tg)))
            .expect("unable to send talkgroup");
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
                // FIXME: non-lexical borrowing
                let event = self.policy.handle_nid(nid);
                self.handle_policy(event);
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
                    TsbkOpcode::GroupVoiceGrant => {
                        let grant = tsbk::GroupVoiceGrant::new(tsbk);
                        self.add_talkgroup(grant.talkgroup(), grant.channel());
                    },
                    TsbkOpcode::GroupVoiceUpdate => {
                        self.handle_traffic_updates(
                            &fields::GroupTrafficUpdate::new(tsbk.payload()));
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
                // FIXME: non-lexical borrowing
                let event = self.policy.handle_call_term();
                self.handle_policy(event);
            },
            LinkControlOpcode::GroupVoiceUpdate => {
                self.handle_traffic_updates(
                    &fields::GroupTrafficUpdate::new(lc.payload()));

                if let Some((tg, freq)) = self.talkgroups.select_preempt() {
                    self.select_talkgroup(tg, freq);
                }
            },
            _ => {},
        }
    }

    fn handle_traffic_updates(&mut self, u: &fields::GroupTrafficUpdate) {
        for &(ch, tg) in u.updates().iter() {
            self.add_talkgroup(tg, ch);
        }
    }

    fn handle_crypto(&mut self, alg: CryptoAlgorithm) {
        if let CryptoAlgorithm::Unencrypted = alg {
            return;
        }

        self.switch_control();

        if let TalkGroup::Other(x) = self.curgroup {
            self.talkgroups.record_encrypted(x);
        }
    }

    fn add_talkgroup(&mut self, tg: TalkGroup, ch: Channel) {
        let tg = match tg {
            TalkGroup::Other(x) => x,
            _ => return,
        };

        let freq = match self.channels.lookup(ch.id()) {
            Some(p) => p.rx_freq(ch.number()),
            None => return,
        };

        self.talkgroups.add_talkgroup(tg, freq);
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
