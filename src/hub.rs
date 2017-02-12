use p25::trunking::fields::TalkGroup;
use sigpower::smeter::SignalLevel;
use std::sync::mpsc::{Sender, Receiver};

use recv::ReceiverEvent;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum HubEvent {
    SetTalkGroup(TalkGroup),
    SetSignalLevel(SignalLevel),
    SetFreq(u32),
}

struct AppState {
    pub talkgroup: TalkGroup,
    pub freq: u32,
    pub signal: SignalLevel,
}

pub struct MainApp {
    state: AppState,
    events: Receiver<HubEvent>,
    recv: Sender<ReceiverEvent>,
}

impl MainApp {
    pub fn new(events: Receiver<HubEvent>, recv: Sender<ReceiverEvent>) -> Self {
        MainApp {
            state: AppState {
                talkgroup: TalkGroup::Nobody,
                freq: 0,
                signal: SignalLevel::None,
            },
            events: events,
            recv: recv,
        }
    }

    pub fn run(&mut self) {
        loop {
            let event = self.events.recv().expect("unable to receive UI event");
            self.handle(event);
        }
    }

    fn handle(&mut self, event: HubEvent) {
        match event {
            HubEvent::SetTalkGroup(tg) => self.state.talkgroup = tg,
            HubEvent::SetSignalLevel(s) => self.state.signal = s,
            HubEvent::SetFreq(freq) =>  self.state.freq = freq,
        }
    }
}
