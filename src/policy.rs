//! Receiver policy state machine.

use p25::message::nid::NetworkId;
use p25::message::nid::DataUnit::*;

use self::PolicyEvent::*;
use self::ReceiverState::*;
use self::StateChange::*;

/// Action that the receiver should take.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum PolicyEvent {
    /// Resynchronize the stream.
    Resync,
    /// Return to the control channel.
    ReturnControl,
    /// Choose a new talkgroup.
    ChooseTalkgroup,
}

/// Current state of receiver.
#[derive(Copy, Clone)]
enum ReceiverState {
    /// On the control channel with a talkgroup selection timer.
    Control(Timer),
    /// On a traffic channel with a watchdog timer.
    ///
    /// The second argument indicates whether the receiver has seen a voice-related
    /// packet.
    Traffic(Timer, bool),
    /// Pausing after a call termination.
    Paused(Timer),
}

/// How the state machine should change/output.
enum StateChange {
    Change(ReceiverState),
    Event(PolicyEvent),
    NoChange,
}

/// Policy state machine for P25 receiver.
pub struct ReceiverPolicy {
    /// Current state.
    state: ReceiverState,
    /// Talkgroup selection timeout.
    select_time: usize,
    /// Watchdog timeout.
    watchdog_time: usize,
    /// Call term pause timeout.
    pause_time: usize,
}

impl ReceiverPolicy {
    /// Create a new `ReceiverPolicy` with the given talkgroup selection timeout, watchdog
    /// timeout, and call termination pause timeout.
    ///
    /// Each timeout should be given as an amount of baseband samples.
    ///
    /// The policy is initialized to start on the control channel.
    pub fn new(select: usize, watchdog: usize, pause: usize) -> Self {
        ReceiverPolicy {
            state: Control(Timer::new(select)),
            select_time: select,
            watchdog_time: watchdog,
            pause_time: pause,
        }
    }

    /// Record a given elapsed amount of baseband samples.
    pub fn handle_elapsed(&mut self, samples: usize) -> Option<PolicyEvent> {
        // FIXME: non-lexical borrowing
        let next = match self.state {
            Control(ref mut t) => if t.expired(samples) {
                t.reset();
                Event(ChooseTalkgroup)
            } else {
                NoChange
            },
            Traffic(ref mut t, _) | Paused(ref mut t) => if t.expired(samples) {
                debug!("watchdog timeout");
                Event(ReturnControl)
            } else {
                NoChange
            },
        };

        self.handle_change(next)
    }

    /// Record a received NID word.
    pub fn handle_nid(&mut self, nid: NetworkId) -> Option<PolicyEvent> {
        // FIXME: non-lexical borrowing
        let next = match self.state {
            Control(..) => NoChange,
            Traffic(_, true) => match nid.data_unit {
                // Ignore (until watchdog timeout) terminators leftover from the previous
                // voice message when initially switching to a traffic channel.
                VoiceLCTerminator | VoiceSimpleTerminator => NoChange,
                // Start processing traffic normally after receiving the first voice
                // header or voice frame.
                VoiceHeader | VoiceLCFrameGroup | VoiceCCFrameGroup => {
                    debug!("receiving voice message");
                    Change(self.state_traffic(false))
                },
                // Ignore spurious TSBKs that occur immediately after switching to a
                // traffic channel. The NID for these gets decoded from the control
                // channel backlog samples buffered by librtlsdr, but an unrecoverable
                // Viterbi error typically follows due to the frequency switch causing a
                // change in stream in the middle of the packet.
                TrunkingSignaling => Event(Resync),
                _ => NoChange,
            },
            Traffic(ref mut t, false) => match nid.data_unit {
                VoiceLCTerminator | VoiceSimpleTerminator => {
                    debug!("pausing for voice message continuation");
                    Change(Paused(Timer::new(self.pause_time)))
                },
                VoiceHeader | VoiceLCFrameGroup | VoiceCCFrameGroup => {
                    // Let the watchdog know that voice packets are still being received.
                    t.reset();
                    NoChange
                },
                _ => NoChange,
            },
            Paused(..) => match nid.data_unit {
                VoiceHeader | VoiceLCFrameGroup | VoiceCCFrameGroup => {
                    debug!("resuming voice message");
                    Change(self.state_traffic(true))
                },
                _ => NoChange,
            }
        };

        self.handle_change(next)
    }

    /// Record a received `CallTermination` indicator.
    pub fn handle_call_term(&mut self) -> Option<PolicyEvent> {
        self.handle_change(match self.state {
            // Ignore spurious term messages that may occur immediately after switching to
            // the control channel.
            Control(..) => NoChange,
            // The call terminator indicates that the current voice message won't be
            // continued.
            Traffic(..) | Paused(..) => {
                debug!("voice message terminated");
                Event(ReturnControl)
            },
        })
    }

    /// Indicate the receiver has moved to a traffic channel.
    pub fn enter_traffic(&mut self) {
        self.state = self.state_traffic(true);
    }

    /// Indicate the receiver has moved to the control channel.
    pub fn enter_control(&mut self) {
        self.state = self.state_control();
    }

    /// Apply the given state change.
    fn handle_change(&mut self, c: StateChange) -> Option<PolicyEvent> {
        match c {
            Change(s) => {
                self.state = s;
                None
            },
            Event(e) => Some(e),
            NoChange => None,
        }
    }

    /// Create a `Control` state.
    fn state_control(&self) -> ReceiverState {
        Control(Timer::new(self.select_time))
    }

    /// Create a `Traffic` state.
    fn state_traffic(&self, init: bool) -> ReceiverState {
        Traffic(Timer::new(self.watchdog_time), init)
    }
}

/// Tracks elapsed time compared to a timeout.
#[derive(Copy, Clone)]
struct Timer {
    /// Timeout value.
    max: usize,
    /// Current value.
    cur: usize,
}

impl Timer {
    /// Create a new `Timer` with the given timeout.
    pub fn new(max: usize) -> Self {
        Timer {
            max: max,
            cur: 0,
        }
    }

    /// Add the given number of samples to the elapsed time and return whether the timer
    /// has timed out.
    pub fn expired(&mut self, samples: usize) -> bool {
        self.cur += samples;
        self.cur >= self.max
    }

    /// Reset the elapsed time to zero.
    pub fn reset(&mut self) {
        self.cur = 0;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use p25::message::nid::NetworkAccessCode;

    #[test]
    fn test_policy() {
        let mut p = ReceiverPolicy::new(10, 20, 30);
        assert_eq!(p.handle_elapsed(9), None);
        assert_eq!(p.handle_elapsed(1), Some(ChooseTalkgroup));
        assert_eq!(p.handle_elapsed(1), None);
        assert_eq!(p.handle_elapsed(9), Some(ChooseTalkgroup));

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_elapsed(15), Some(ReturnControl));

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCTerminator)), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceSimpleTerminator)), None);
        assert_eq!(p.handle_elapsed(15), Some(ReturnControl));

        p.enter_control();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_elapsed(5), Some(ChooseTalkgroup));

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceHeader)), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCFrameGroup)), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceCCFrameGroup)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_elapsed(1), Some(ReturnControl));
        p.enter_control();

        p.enter_traffic();
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, TrunkingSignaling)),
            Some(Resync));

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_call_term(), Some(ReturnControl));
        p.enter_control();

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceHeader)), None);
        assert_eq!(p.handle_call_term(), Some(ReturnControl));
        p.enter_control();

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceHeader)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCFrameGroup)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceCCFrameGroup)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceSimpleTerminator)), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCTerminator)), None);
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceSimpleTerminator)), None);
        assert_eq!(p.handle_elapsed(24), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCTerminator)), None);
        assert_eq!(p.handle_elapsed(21), Some(ReturnControl));
        p.enter_control();

        p.enter_traffic();
        assert_eq!(p.handle_elapsed(5), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceHeader)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceLCFrameGroup)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceCCFrameGroup)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceSimpleTerminator)), None);
        assert_eq!(p.handle_elapsed(29), None);
        assert_eq!(p.handle_nid(
            NetworkId::new(NetworkAccessCode::Default, VoiceHeader)), None);
        assert_eq!(p.handle_elapsed(19), None);
        assert_eq!(p.handle_elapsed(1), Some(ReturnControl));
    }
}
