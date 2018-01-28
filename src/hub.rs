//! HTTP REST interface and event streaming.

use std::convert::TryFrom;
use std::io::{Write, ErrorKind};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::{RawFd, FromRawFd, IntoRawFd};
use std::sync::mpsc::{Sender, TryRecvError};
use std;

use arrayvec::ArrayVec;
use mio::tcp::TcpListener;
use mio::unix::EventedFd;
use mio::{Poll, PollOpt, Token, Event, Events, Ready};
use mio_more::channel::Receiver;
use p25::stats::{CodeStats, Stats};
use p25::trunking::fields::{self, ChannelParamsMap};
use p25::trunking::tsbk::{self, TsbkFields, TsbkOpcode};
use p25::voice::control::{self, LinkControlFields, LinkControlOpcode};
use p25::voice::crypto::CryptoAlgorithm;
use serde::Serialize;
use serde_json;
use uhttp_json_api::{HttpRequest, HttpResult};
use uhttp_method::Method;
use uhttp_response_header::HeaderLines;
use uhttp_sse::SseMessage;
use uhttp_status::StatusCode;
use uhttp_uri::HttpResource;
use uhttp_version::HttpVersion;

use http;
use recv::RecvEvent;
use talkgroups::GroupCryptoMap;

/// Available routes.
enum Route {
    /// Subscribe to SSE stream.
    Subscribe,
    /// Get/Set control channel frequency.
    CtlFreq,
    /// Get current known encrypted talkgroups.
    Encrypted,
    /// Reset stat counters.
    ResetStats,
}

impl<'a> TryFrom<HttpResource<'a>> for Route {
    type Error = StatusCode;

    fn try_from(r: HttpResource<'a>) -> HttpResult<Self> {
        match r.path {
            "/subscribe" => Ok(Route::Subscribe),
            "/ctlfreq" => Ok(Route::CtlFreq),
            "/encrypted" => Ok(Route::Encrypted),
            "/stats/reset" => Ok(Route::ResetStats),
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

/// Masks off token tag.
const TAG_MASK: usize = !(FD_MASK as usize);

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
        match tok.0 & TAG_MASK {
            CONNS => HubToken::Conns,
            EVENTS => HubToken::Events,
            REQUEST => HubToken::Request(tok.0 as RawFd & FD_MASK),
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
            (Method::Get, Route::Encrypted) => {
                http::send_json(req.into_stream(), json!({
                    "encrypted": &self.state.encrypted,
                })).is_ok();

                Ok(())
            },
            (Method::Put, Route::ResetStats) => {
                self.recv.send(RecvEvent::ResetStats)
                    .expect("unable to reset stats");

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

    fn stream_event(&mut self, s: &mut TcpStream, e: &HubEvent) -> Result<(), ()> {
        use self::HubEvent::*;
        use self::StateEvent::*;

        match *e {
            State(UpdateCtlFreq(f)) => SerdeEvent::new("ctlFreq", f).write(s),
            State(UpdateChannelParams(_)) => Ok(()),
            State(UpdateEncrypted(..)) =>
                SerdeEvent::new("updateEncrypted", &self.state.encrypted).write(s),
            UpdateCurFreq(f) => SerdeEvent::new("curFreq", f).write(s),
            UpdateTalkGroup(tg) => SerdeEvent::new("talkGroup", tg).write(s),
            UpdateSignalPower(p) => SerdeEvent::new("sigPower", p).write(s),
            // If this event has been received, the TSBK is valid with a known opcode.
            TrunkingControl(tsbk) => match tsbk.opcode().unwrap() {
                TsbkOpcode::RfssStatusBroadcast => self.stream_rfss_status(s,
                    fields::RfssStatusBroadcast::new(tsbk.payload())),
                TsbkOpcode::NetworkStatusBroadcast => self.stream_net_status(s,
                    fields::NetworkStatusBroadcast::new(tsbk.payload())),
                TsbkOpcode::AltControlChannel => self.stream_alt_control(s,
                    fields::AltControlChannel::new(tsbk.payload())),
                TsbkOpcode::AdjacentSite => self.stream_adjacent_site(s,
                    fields::AdjacentSite::new(tsbk.payload())),
                TsbkOpcode::LocRegResponse => {
                    let f = tsbk::LocRegResponse::new(tsbk);

                    SerdeEvent::new("locReg", json!({
                        "response": f.response(),
                        "rfss": f.rfss(),
                        "site": f.site(),
                        "unit": f.dest_unit(),
                    })).write(s)
                },
                TsbkOpcode::UnitRegResponse => {
                    let f = tsbk::UnitRegResponse::new(tsbk);

                    SerdeEvent::new("unitReg", json!({
                        "response": f.response(),
                        "system": f.system(),
                        "unitId": f.src_id(),
                        "unitAddr": f.src_addr(),
                    })).write(s)
                },
                TsbkOpcode::UnitDeregAck => {
                    let f = tsbk::UnitDeregAck::new(tsbk);

                    SerdeEvent::new("unitDereg", json!({
                        "wacn": f.wacn(),
                        "system": f.system(),
                        "unit": f.src_unit(),
                    })).write(s)
                },
                _ => Ok(()),
            },
            // If this event has been received, the LC has a known opcode.
            LinkControl(lc) => match lc.opcode().unwrap() {
                LinkControlOpcode::GroupVoiceTraffic =>
                    SerdeEvent::new("srcUnit",
                        control::GroupVoiceTraffic::new(lc).src_unit()).write(s),
                LinkControlOpcode::RfssStatusBroadcast => self.stream_rfss_status(s,
                    fields::RfssStatusBroadcast::new(lc.payload())),
                LinkControlOpcode::NetworkStatusBroadcast => self.stream_net_status(s,
                    fields::NetworkStatusBroadcast::new(lc.payload())),
                LinkControlOpcode::AdjacentSite => self.stream_adjacent_site(s,
                    fields::AdjacentSite::new(lc.payload())),
                LinkControlOpcode::AltControlChannel => self.stream_alt_control(s,
                    fields::AltControlChannel::new(lc.payload())),
                _ => Ok(()),
            },
            UpdateStats(stats) =>
                SerdeEvent::new("updateStats", serialize_stats(&stats)).write(s),
        }
    }

    fn stream_rfss_status(&self, s: &mut TcpStream, f: fields::RfssStatusBroadcast)
        -> Result<(), ()>
    {
        SerdeEvent::new("rfssStatus", json!({
            "area": f.area(),
            "system": f.system(),
            "rfss": f.rfss(),
            "site": f.site(),
        })).write(s)
    }

    fn stream_net_status(&self, s: &mut TcpStream, f: fields::NetworkStatusBroadcast)
        -> Result<(), ()>
    {
        SerdeEvent::new("networkStatus", json!({
            "area": f.area(),
            "wacn": f.wacn(),
            "system": f.system(),
        })).write(s)
    }

    fn stream_alt_control(&self, mut s: &mut TcpStream, f: fields::AltControlChannel)
        -> Result<(), ()>
    {
        for &(ch, _) in f.alts().iter() {
            let freq = match self.state.channels.lookup(ch.id()) {
                Some(p) => p.rx_freq(ch.number()),
                None => continue,
            };

            try!(SerdeEvent::new("altControl", json!({
                "rfss": f.rfss(),
                "site": f.site(),
                "freq": freq,
            })).write(&mut s));
        }

        Ok(())
    }

    fn stream_adjacent_site(&self, s: &mut TcpStream, f: fields::AdjacentSite)
        -> Result<(), ()>
    {
        let ch = f.channel();

        let freq = match self.state.channels.lookup(ch.id()) {
            Some(p) => p.rx_freq(ch.number()),
            None => return Ok(()),
        };

        SerdeEvent::new("adjacentSite", json!({
            "area": f.area(),
            "rfss": f.rfss(),
            "system": f.system(),
            "site": f.site(),
            "freq": freq,
        })).write(s)
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
    UpdateTalkGroup(u16),
    /// Power of received signal.
    UpdateSignalPower(f32),
    /// Trunking control packet was received.
    TrunkingControl(TsbkFields),
    /// Link control packet was received.
    LinkControl(LinkControlFields),
    /// Updated stat counters.
    UpdateStats(Stats),
}

/// State update events.
#[derive(Copy, Clone)]
pub enum StateEvent {
    /// Control channel frequency has been committed.
    UpdateCtlFreq(u32),
    /// Channel parameters have been modified.
    UpdateChannelParams(TsbkFields),
    /// Encrypted talkgroup encountered.
    UpdateEncrypted(u16, CryptoAlgorithm),
}

/// Holds a copy of certain state held in other tasks.
pub struct State {
    /// Current control channel frequency.
    ctlfreq: u32,
    /// Channel parameters for current site.
    channels: ChannelParamsMap,
    /// Known encrypted talkgroups.
    encrypted: GroupCryptoMap,
}

impl Default for State {
    fn default() -> Self {
        State {
            ctlfreq: std::u32::MAX,
            channels: ChannelParamsMap::default(),
            encrypted: GroupCryptoMap::default(),
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
            UpdateEncrypted(tg, alg) => { self.encrypted.insert(tg, alg); },
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

fn serialize_stats(s: &Stats) -> impl Serialize {
    json!({
        "bch": serialize_code_stats(&s.bch),
        "cyclic": serialize_code_stats(&s.cyclic),
        "golayStd": serialize_code_stats(&s.golay_std),
        "golayExt": serialize_code_stats(&s.golay_ext),
        "golayShort": serialize_code_stats(&s.golay_short),
        "hammingStd": serialize_code_stats(&s.hamming_std),
        "hammingShort": serialize_code_stats(&s.hamming_short),
        "rsShort": serialize_code_stats(&s.rs_short),
        "rsMed": serialize_code_stats(&s.rs_med),
        "rsLong": serialize_code_stats(&s.rs_long),
        "viterbiDibit": serialize_code_stats(&s.viterbi_dibit),
        "viterbiTribit": serialize_code_stats(&s.viterbi_tribit),
    })
}

fn serialize_code_stats(s: &CodeStats) -> impl Serialize {
    json!({
        "totalWords": s.words,
        "errWords": s.errs,
        "totalSymbols": s.words * s.size,
        "fixedSymbols": s.fixed,
    })
}
