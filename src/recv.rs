//! Receiver logic.

use std::sync::mpsc::{Sender, Receiver};
use std;

use mio_more;
use p25::message::receiver::MessageReceiver;
use p25::stats::Stats;
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap, Channel};
use p25::trunking::tsbk::{self, TsbkOpcode, TsbkFields};
use p25::voice::control::LinkControlFields;
use p25::voice::crypto::CryptoAlgorithm;
use pool::Checkout;

use audio::AudioEvent;
use hub::{HubEvent, StateEvent};
use policy::{ReceiverPolicy, PolicyEvent};
use sdr::ControlTaskEvent;
use talkgroups::TalkgroupSelection;

/// Messages for `RecvTask`.
pub enum RecvEvent {
    /// Chunk of baseband samples.
    Baseband(Checkout<Vec<f32>>),
    /// Change the control channel frequency.
    SetControlFreq(u32),
}

/// Processes P25 baseband and performs the duties of a trunking receiver.
pub struct RecvTask {
    /// Receiver events.
    events: Receiver<RecvEvent>,
    /// Event streaming.
    hub: mio_more::channel::Sender<HubEvent>,
    /// SDR control task.
    sdr: Sender<ControlTaskEvent>,
    /// Audio output task.
    audio: Sender<AudioEvent>,
    /// Control channel frequency (Hz).
    ctlfreq: u32,
    /// Whether frequency hopping is enabled.
    hopping: bool,
    /// Receiver state machine.
    msg: MessageReceiver,
    /// Policy state machine.
    policy: ReceiverPolicy,
    /// Talkgroup selection machinery.
    talkgroups: TalkgroupSelection,
    /// Channel mappings.
    channels: ChannelParamsMap,
    /// Current center frequency (Hz).
    curfreq: u32,
    /// Current talkgroup being monitored.
    curgroup: u16,
    /// Accumlated statistics.
    stats: Stats,
}

impl RecvTask {
    /// Create a new `RecvTask`.
    pub fn new(events: Receiver<RecvEvent>,
               hub: mio_more::channel::Sender<HubEvent>,
               sdr: Sender<ControlTaskEvent>,
               audio: Sender<AudioEvent>,
               ctlfreq: u32,
               hopping: bool,
               policy: ReceiverPolicy,
               talkgroups: TalkgroupSelection)
        -> Self
    {
        RecvTask {
            events: events,
            hub: hub,
            sdr: sdr,
            audio: audio,
            ctlfreq: std::u32::MAX,
            hopping: hopping,
            msg: MessageReceiver::new(),
            policy: policy,
            talkgroups: talkgroups,
            channels: ChannelParamsMap::default(),
            curfreq: std::u32::MAX,
            curgroup: 0,
            stats: Stats::default(),
        }.init(ctlfreq)
    }

    /// Finalize initialization of the receiver.
    fn init(mut self, freq: u32) -> Self {
        self.set_control_freq(freq);
        self
    }

    /// Change the control channel frequency (Hz).
    ///
    /// This will immediately switch to the new control channel.
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

    /// Move to the control channel.
    fn switch_control(&mut self) {
        self.audio.send(AudioEvent::EndTransmission)
            .expect("unable to send end of transmission");

        // FIXME: non-lexical borrowing
        let freq = self.ctlfreq;
        self.set_freq(freq);

        self.policy.enter_control();
    }

    /// Move to the given frequency (Hz).
    fn set_freq(&mut self, freq: u32) {
        debug!("moving to frequency {} Hz", freq);
        self.curfreq = freq;

        self.hub.send(HubEvent::UpdateCurFreq(freq))
            .expect("unable to send current frequency");
        self.sdr.send(ControlTaskEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");

        self.msg.resync();
    }

    /// Begin processing baseband samples, blocking the current thread.
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

    /// Handle the given policy event.
    fn handle_policy(&mut self, e: Option<PolicyEvent>) {
        use self::PolicyEvent::*;

        let event = match e {
            Some(e) => e,
            None => return,
        };

        match event {
            Resync => self.msg.resync(),
            ReturnControl => self.switch_control(),
            ChooseTalkgroup => {
                if let Some((tg, freq)) = self.talkgroups.select_idle() {
                    self.select_talkgroup(tg, freq);
                }
            },
        }
    }

    /// Choose the given talkgroup as the next to monitor.
    fn select_talkgroup(&mut self, tg: u16, freq: u32) {
        if !self.hopping {
            return;
        }

        self.curgroup = tg;
        self.set_freq(freq);
        self.policy.enter_traffic();

        self.hub.send(HubEvent::UpdateTalkGroup(self.curgroup))
            .expect("unable to send talkgroup");
    }

    /// Process the given baseband sample.
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
                trace!("received NID {:?}", nid.data_unit);

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
            TrunkingControl(tsbk) => self.handle_tsbk(tsbk),
            VoiceTerm(lc) => self.handle_lc(lc),
        }
    }

    /// Process the given trunking packet.
    fn handle_tsbk(&mut self, tsbk: TsbkFields) {
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

        trace!("received TSBK with opcode {:?}", opcode);

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
    }

    /// Process the given link control word.
    fn handle_lc(&mut self, lc: LinkControlFields) {
        use p25::voice::control::LinkControlOpcode;

        let opcode = match lc.opcode() {
            Some(o) => o,
            None => return,
        };

        trace!("received LC word with opcode {:?}", opcode);

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

    /// Collect talkgroups from the given traffic update packet.
    fn handle_traffic_updates(&mut self, u: &fields::GroupTrafficUpdate) {
        for &(ch, tg) in u.updates().iter() {
            self.add_talkgroup(tg, ch);
        }
    }

    /// Process the given encryption info for the current talkgroup.
    fn handle_crypto(&mut self, alg: CryptoAlgorithm) {
        if let CryptoAlgorithm::Unencrypted = alg {
            return;
        }

        self.switch_control();
        self.talkgroups.record_encrypted(self.curgroup);
    }

    /// Collect the given talkgroup and associated traffic channel.
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
