use p25::trunking::fields::TalkGroup;
use pi25_cfg::sites::P25Sites;
use sigpower::smeter::SignalLevel;
use std::sync::Arc;
use std::sync::mpsc::{Sender, Receiver};

use recv::ReceiverEvent;
use sdr::ControllerEvent;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum UIEvent {
    SetTalkGroup(TalkGroup),
    SetSignalLevel(SignalLevel),
    SetFreq(u32),
}

struct AppState {
    pub sites: Arc<P25Sites>,
    pub site: usize,
    pub talkgroup: TalkGroup,
    pub freq: u32,
    pub signal: SignalLevel,
}

pub struct MainApp {
    state: AppState,
    events: Receiver<UIEvent>,
    sdr: Sender<ControllerEvent>,
    recv: Sender<ReceiverEvent>,
}

impl MainApp {
    pub fn new(sites: Arc<P25Sites>,
               site: usize,
               events: Receiver<UIEvent>,
               sdr: Sender<ControllerEvent>,
               recv: Sender<ReceiverEvent>)
        -> MainApp
    {
        MainApp {
            state: AppState {
                sites: sites,
                site: site,
                talkgroup: TalkGroup::Nobody,
                freq: 0,
                signal: SignalLevel::None,
            },
            events: events,
            sdr: sdr,
            recv: recv,
        }.init()
    }

    fn init(self) -> Self {
        self.commit_site();
        self
    }

    fn commit_site(&self) {
        self.recv.send(ReceiverEvent::SetControlFreq(
            self.state.sites[self.state.site].control
        )).expect("unable to commit site");
    }

    pub fn run(&mut self) {
        loop {
            let event = self.events.recv().expect("unable to receive UI event");
            self.handle(event);
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
