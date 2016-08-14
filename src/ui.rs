use cfg::sites::P25Sites;
use cfg::talkgroups::TalkGroups;
use p25::trunking::decode::TalkGroup;
use rtlsdr::TunerGains;
use sigpower::smeter::SignalLevel;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{Sender, Receiver};

use consts::DEFAULT_SITE;
use recv::ReceiverEvent;
use sdr::ControllerEvent;

const MIN_VOL: usize = 80;
const MAX_VOL: usize = 100;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum UIEvent {
    SetTalkGroup(TalkGroup),
    SetSignalLevel(SignalLevel),
    SetFreq(u32),
}

struct AppState {
    pub gains: (TunerGains, usize),
    pub sites: Arc<P25Sites>,
    pub volume: usize,
    /// Index into gains array.
    pub gain: usize,
    pub site: usize,
    pub talkgroup: TalkGroup,
    pub freq: u32,
    pub signal: SignalLevel,
}

pub struct MainApp {
    state: AppState,
    talkgroups: TalkGroups,
    events: Receiver<UIEvent>,
    sdr: Sender<ControllerEvent>,
    recv: Sender<ReceiverEvent>,
}

impl MainApp {
    pub fn new(talkgroups: TalkGroups,
               sites: Arc<P25Sites>,
               gains: (TunerGains, usize),
               events: Receiver<UIEvent>,
               sdr: Sender<ControllerEvent>,
               recv: Sender<ReceiverEvent>)
        -> MainApp
    {
        MainApp {
            state: AppState {
                gains: gains,
                sites: sites,
                volume: (MAX_VOL + MIN_VOL) / 2,
                gain: gains.1 - 1,
                site: DEFAULT_SITE,
                talkgroup: TalkGroup::Nobody,
                freq: 0,
                signal: SignalLevel::None,
            },
            talkgroups: talkgroups,
            events: events,
            sdr: sdr,
            recv: recv,
        }.init()
    }

    fn init(mut self) -> Self {
        self.commit_gain();
        self.commit_site();
        self.commit_volume();

        self
    }

    pub fn redraw(&mut self) {}

    fn commit_volume(&self) {
        assert!(Command::new("amixer")
                        .arg("sset")
                        .arg("PCM")
                        .arg(format!("{}%", self.state.volume))
                        .status()
                        .unwrap()
                        .success());
    }

    fn commit_site(&self) {
        self.recv.send(ReceiverEvent::SetSite(self.state.site))
            .expect("unable to commit site");
    }

    fn commit_gain(&mut self) {
        self.sdr.send(ControllerEvent::SetGain(self.state.gains.0[self.state.gain]))
            .expect("unable to commit gain");
    }

    fn poweroff(&mut self) {
        assert!(Command::new("sudo")
                        .arg("systemctl")
                        .arg("poweroff")
                        .status()
                        .unwrap()
                        .success());
    }

    pub fn run(&mut self) {
        loop {
            let event = self.events.recv().expect("unable to receive UI event");
            self.handle(event);
            self.redraw();
        }
    }

    fn handle(&mut self, event: UIEvent) {
        match event {
            UIEvent::SetTalkGroup(tg) => self.state.talkgroup = tg,
            UIEvent::SetSignalLevel(s) => self.state.signal = s,
            UIEvent::SetFreq(freq) =>  self.state.freq = freq,
        }
    }
}
