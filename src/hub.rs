//! HTTP REST interface and event streaming.

use std::convert::TryFrom;
use std::io::{Write, ErrorKind};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::{RawFd, FromRawFd, IntoRawFd};
use std::sync::mpsc::{Sender, TryRecvError};
use std;

use arrayvec::ArrayVec;
use mio::channel::{Receiver};
use mio::{Poll, PollOpt, Token, Event, Events, Ready};
use mio::tcp::TcpListener;
use mio::unix::EventedFd;
use p25::trunking::fields::{self, TalkGroup, ChannelParamsMap};
use p25::trunking::tsbk::{TsbkFields, TsbkOpcode};
use p25::voice::control::{self, LinkControlFields, LinkControlOpcode};
use serde_json;
use serde::Serialize;
use uhttp_json_api::{HttpRequest, HttpResult};
use uhttp_method::Method;
use uhttp_response_header::HeaderLines;
use uhttp_sse::SseMessage;
use uhttp_status::StatusCode;
use uhttp_uri::HttpResource;
use uhttp_version::HttpVersion;

use http;
use recv::RecvEvent;

/// Available routes.
enum Route {
    /// Subscribe to SSE stream.
    Subscribe,
    /// Get/Set control channel frequency.
    CtlFreq,
}

impl<'a> TryFrom<HttpResource<'a>> for Route {
    type Error = StatusCode;

    fn try_from(r: HttpResource<'a>) -> HttpResult<Self> {
        match r.path {
            "/subscribe" => Ok(Route::Subscribe),
            "/ctlfreq" => Ok(Route::CtlFreq),
            _ => Err(StatusCode::NotFound),
        }
    }
}

const CONNS: usize = 1 << 31;
const EVENTS: usize = 1 << 30;
const REQUEST: usize = 1 << 29;

/// Allow 24 bits for file descriptors
///
/// This assumes file descriptors don't require the full 32 bits, which seems like a
/// safe assumption (http://unix.stackexchange.com/questions/84227).
const FD_MASK: RawFd = (1 << 24) - 1;

/// Async event types.
///
/// The complications around packing this type into 32-bit `Token`s is to support
/// platforms with 32-bit `usize`.
pub enum HubToken {
    /// Socket connection.
    Conns,
    /// Channel events.
    Events,
    /// Request stream with contained file descriptor.
    Request(RawFd),
}

impl From<HubToken> for Token {
    fn from(tok: HubToken) -> Self {
        Token(match tok {
            HubToken::Conns => CONNS,
            HubToken::Events => EVENTS,
            HubToken::Request(fd) => REQUEST | fd as usize
        })
    }
}

impl From<Token> for HubToken {
    fn from(tok: Token) -> Self {
        match tok.0 {
            CONNS => HubToken::Conns,
            EVENTS => HubToken::Events,
            b if b & REQUEST != 0 => HubToken::Request(b as RawFd & FD_MASK),
            _ => panic!("unknown token"),
        }
    }
}

impl HubToken {
    pub fn for_request(fd: RawFd) -> Self {
        assert!(fd & !FD_MASK == 0);
        HubToken::Request(fd)
    }
}

/// Handles HTTP requests and broadcasts events to listening subscribers.
pub struct HubTask {
    /// Tracks pertinent state of other tasks.
    state: State,
    /// Main socket for HTTP connections.
    socket: TcpListener,
    /// Async event loop.
    events: Poll,
    /// Streams subscribed to receive events.
    streamers: ArrayVec<[TcpStream; 4]>,
    /// Channel for receiving events.
    chan: Receiver<HubEvent>,
    /// Channel for communication with RecvTask.
    recv: Sender<RecvEvent>,
}

impl HubTask {
    /// Create a new `HubTask` to communicate on the given channels and bind to the given
    /// socket address.
    pub fn new(chan: Receiver<HubEvent>, recv: Sender<RecvEvent>, addr: &SocketAddr)
        -> std::io::Result<Self>
    {
        let socket = TcpListener::bind(addr)?;
        let events = Poll::new()?;

        try!(events.register(&socket, HubToken::Conns.into(), Ready::readable(),
            PollOpt::edge()));
        try!(events.register(&chan, HubToken::Events.into(), Ready::readable(),
            PollOpt::edge()));

        Ok(HubTask {
            state: State::default(),
            socket: socket,
            events: events,
            streamers: ArrayVec::new(),
            chan: chan,
            recv: recv,
        })
    }

    /// Start handling HTTP requests and events, blocking the current thread.
    pub fn run(&mut self) {
        let mut events = Events::with_capacity(32);

        loop {
            self.events.poll(&mut events, None)
                .expect("unable to poll events");

            for event in events.iter() {
                self.handle_poll(event);
            }
        }
    }

    /// Handle the given event.
    fn handle_poll(&mut self, e: Event) {
        match e.token().into() {
            HubToken::Conns =>
                self.handle_conns().expect("unable to handle connection"),
            HubToken::Events =>
                self.handle_chan().expect("unable to handle channel event"),
            HubToken::Request(fd) => {
                let stream = unsafe { TcpStream::from_raw_fd(fd.into()) };

                self.events.deregister(&EventedFd(&fd))
                    .expect("unable to deregister stream");

                self.handle_stream(stream);
            },
        }
    }

    /// Handle pending HTTP connections.
    fn handle_conns(&mut self) -> Result<(), ()> {
        loop {
            let (stream, _) = match self.socket.accept_std() {
                Ok(x) => x,
                Err(e) => return if e.kind() == ErrorKind::WouldBlock {
                    Ok(())
                } else {
                    Err(())
                },
            };

            let fd = stream.into_raw_fd();
            let tok = HubToken::for_request(fd);
            let event = EventedFd(&fd);

            self.events.register(&event, tok.into(), Ready::readable(), PollOpt::edge())
                .expect("unable to register stream");
        }
    }

    /// Handle pending channel events.
    fn handle_chan(&mut self) -> Result<(), ()> {
        loop {
            match self.chan.try_recv() {
                Ok(e) => self.handle_event(e),
                Err(TryRecvError::Disconnected) => return Err(()),
                Err(TryRecvError::Empty) => return Ok(()),
            }
        }
    }

    /// Handle the given channel event.
    fn handle_event(&mut self, e: HubEvent) {
        if let HubEvent::State(sm) = e {
            self.state.update(sm);
        }

        // Holds streamers that are still alive.
        let mut keep = ArrayVec::<[TcpStream; 4]>::new();

        loop {
            let mut s = match self.streamers.pop() {
                Some(s) => s,
                None => break,
            };

            if let Ok(()) = self.stream_event(&mut s, &e) {
                keep.push(s);
            }
        }

        self.streamers = keep;
    }

    /// Handle the given HTTP connection.
    fn handle_stream(&mut self, mut s: TcpStream) {
        match self.handle_request(&mut s) {
            Ok(()) => {},
            Err(e) => { http::send_status(&mut s, e).is_ok(); }
        }
    }

    fn handle_request(&mut self, s: &mut TcpStream) -> HttpResult<()> {
        let mut buf = [0; 8192];

        let mut req = HttpRequest::new(s, &mut buf[..])?;
        let (ver, method, route) = req.route()?;

        if ver != HttpVersion::from_parts(1, 1) {
            return Err(StatusCode::NotImplemented);
        }

        match (method, route) {
            (Method::Get, Route::Subscribe) => {
                if let Ok(mut s) = req.into_stream().try_clone() {
                    // Check if streamer can be supported before sending response.
                    if self.streamers.is_full() {
                        return Err(StatusCode::TooManyRequests);
                    }

                    if self.start_stream(&mut s).is_ok() {
                        // This is guaranteed to succeed due to the above check.
                        self.streamers.push(s);
                    }

                    Ok(())
                } else {
                    Err(StatusCode::InternalServerError)
                }
            },
            (Method::Get, Route::CtlFreq) => {
                http::send_json(req.into_stream(), SerdeCtlFreq {
                    ctlfreq: self.state.ctlfreq,
                }).is_ok();

                Ok(())
            },
            (Method::Put, Route::CtlFreq) => {
                let msg: SerdeCtlFreq = req.read_json()?;

                // TODO: verify frequency range.

                if self.recv.send(RecvEvent::SetControlFreq(msg.ctlfreq)).is_err() {
                    return Err(StatusCode::InternalServerError);
                }

                http::send_status(req.into_stream(), StatusCode::Ok).is_ok();

                Ok(())
            },
            (Method::Options, _) => {
                let mut h = HeaderLines::new(req.into_stream());

                http::send_head(&mut h, StatusCode::Ok).is_ok();
                write!(h.line(), "Access-Control-Allow-Methods: GET, PUT").is_ok();
                write!(h.line(), "Access-Control-Allow-Headers: Content-Type").is_ok();

                Ok(())
            },
            _ => Err(StatusCode::MethodNotAllowed),
        }
    }

    /// Send the initial streaming header to the given subscriber.
    fn start_stream(&self, s: &mut TcpStream) -> std::io::Result<()> {
        let mut h = HeaderLines::new(s);

        try!(http::send_head(&mut h, StatusCode::Ok));
        try!(write!(h.line(), "Content-Type: text/event-stream"));

        Ok(())
    }

    fn stream_event(&mut self, mut s: &mut TcpStream, e: &HubEvent) -> Result<(), ()> {
        use self::HubEvent::*;
        use self::StateEvent::*;

        match *e {
            State(UpdateCtlFreq(f)) => SerdeEvent::new("ctlFreq", f).write(s),

            State(UpdateChannelParams(_)) => Ok(()),

            UpdateCurFreq(f) => SerdeEvent::new("curFreq", f).write(s),

            UpdateTalkGroup(tg) => SerdeEvent::new("talkGroup", tg).write(s),

            UpdateSignalPower(p) => SerdeEvent::new("sigPower", p).write(s),

            // If this event has been received, the TSBK is valid with a known opcode.
            TrunkingControl(tsbk) => match tsbk.opcode().unwrap() {
                TsbkOpcode::RfssStatusBroadcast =>
                    SerdeEvent::new("rfssStatus", SerdeRfssStatus::new(
                        &fields::RfssStatusBroadcast::new(tsbk.payload()))).write(s),

                TsbkOpcode::NetworkStatusBroadcast =>
                    SerdeEvent::new("networkStatus", SerdeNetworkStatus::new(
                        &fields::NetworkStatusBroadcast::new(tsbk.payload()))).write(s),

                TsbkOpcode::AltControlChannel => {
                    let dec = fields::AltControlChannel::new(tsbk.payload());

                    for &(ch, _) in dec.alts().iter() {
                        let freq = match self.state.channels.lookup(ch.id()) {
                            Some(p) => p.rx_freq(ch.number()),
                            None => continue,
                        };

                        try!(SerdeEvent::new("altControl",
                            SerdeAltControl::new(&dec, freq)).write(&mut s));
                    }

                    Ok(())
                },

                TsbkOpcode::AdjacentSite => {
                    let dec = fields::AdjacentSite::new(tsbk.payload());
                    let ch = dec.channel();

                    let freq = match self.state.channels.lookup(ch.id()) {
                        Some(p) => p.rx_freq(ch.number()),
                        None => return Ok(()),
                    };

                    SerdeEvent::new("adjacentSite",
                        SerdeAdjacentSite::new(&dec, freq)).write(s)
                },

                _ => Ok(()),
            },

            // If this event has been received, the LC has a known opcode.
            LinkControl(lc) => match lc.opcode().unwrap() {
                LinkControlOpcode::GroupVoiceTraffic =>
                    SerdeEvent::new("srcUnit",
                        control::GroupVoiceTraffic::new(lc).src_unit()).write(s),

                _ => Ok(()),
            }
        }
    }
}

/// Events for the hub.
#[derive(Clone)]
pub enum HubEvent {
    /// Some state update.
    State(StateEvent),
    /// Center frequency was changed.
    UpdateCurFreq(u32),
    /// Current talkgroup has changed.
    UpdateTalkGroup(TalkGroup),
    /// Power of received signal.
    UpdateSignalPower(f32),
    /// Trunking control packet was received.
    TrunkingControl(TsbkFields),
    /// Link control packet was received.
    LinkControl(LinkControlFields),
}

/// State update events.
#[derive(Copy, Clone)]
pub enum StateEvent {
    /// Control channel frequency has been committed.
    UpdateCtlFreq(u32),
    /// Channel parameters have been modified.
    UpdateChannelParams(TsbkFields),
}

/// Holds a copy of certain state held in other tasks.
pub struct State {
    /// Current control channel frequency.
    ctlfreq: u32,
    /// Channel parameters for current site.
    channels: ChannelParamsMap,
}

impl Default for State {
    fn default() -> Self {
        State {
            ctlfreq: std::u32::MAX,
            channels: ChannelParamsMap::default(),
        }
    }
}

impl State {
    /// Update the state based on the given event.
    fn update(&mut self, e: StateEvent) {
        use self::StateEvent::*;

        match e {
            UpdateCtlFreq(f) => self.ctlfreq = f,
            UpdateChannelParams(tsbk) =>
                self.channels.update(&fields::ChannelParamsUpdate::new(tsbk.payload())),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct SerdeCtlFreq {
    ctlfreq: u32,
}

#[derive(Serialize)]
struct SerdeEvent<T: Serialize> {
    event: &'static str,
    payload: T,
}

impl<T: Serialize> SerdeEvent<T> {
    pub fn new(event: &'static str, payload: T) -> Self {
        SerdeEvent {
            event: event,
            payload: payload,
        }
    }

    pub fn write<W: Write>(&self, stream: W) -> Result<(), ()> {
        let mut msg = SseMessage::new(stream);
        let mut data = msg.data().map_err(|_| ())?;

        serde_json::to_writer(&mut data, self).map_err(|_| ())
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct SerdeRfssStatus {
    area: u8,
    system: u16,
    rfss: u8,
    site: u8,
}

impl SerdeRfssStatus {
    pub fn new(s: &fields::RfssStatusBroadcast) -> Self {
        SerdeRfssStatus {
            area: s.area(),
            system: s.system(),
            rfss: s.rfss(),
            site: s.site(),
        }
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct SerdeNetworkStatus {
    area: u8,
    wacn: u32,
    system: u16,
}

impl SerdeNetworkStatus {
    pub fn new(s: &fields::NetworkStatusBroadcast) -> Self {
        SerdeNetworkStatus {
            area: s.area(),
            wacn: s.wacn(),
            system: s.system(),
        }
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct SerdeAltControl {
    rfss: u8,
    site: u8,
    freq: u32,
}

impl SerdeAltControl {
    pub fn new(s: &fields::AltControlChannel, freq: u32) -> Self {
        SerdeAltControl {
            rfss: s.rfss(),
            site: s.site(),
            freq: freq,
        }
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct SerdeAdjacentSite {
    area: u8,
    rfss: u8,
    system: u16,
    site: u8,
    freq: u32,
}

impl SerdeAdjacentSite {
    pub fn new(s: &fields::AdjacentSite, freq: u32) -> Self {
        SerdeAdjacentSite {
            area: s.area(),
            rfss: s.rfss(),
            system: s.system(),
            site: s.site(),
            freq: freq,
        }
    }
}
