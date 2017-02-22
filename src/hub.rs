use std::convert::TryFrom;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender, Receiver};
use std;

use chan;
use p25::trunking::fields::{TalkGroup, RfssStatusBroadcast, NetworkStatusBroadcast};
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
use recv::ReceiverEvent;

pub enum Route {
    Subscribe,
    CtlFreq,
}

impl<'a> TryFrom<HttpResource<'a>> for Route {
    type Err = StatusCode;

    fn try_from(r: HttpResource<'a>) -> HttpResult<Self> {
        match r.path {
            "/subscribe" => Ok(Route::Subscribe),
            "/ctlfreq" => Ok(Route::CtlFreq),
            _ => Err(StatusCode::NotFound),
        }
    }
}

#[derive(Clone)]
pub enum HubEvent {
    State(StateEvent),
    UpdateCurFreq(u32),
    UpdateTalkGroup(TalkGroup),
    UpdateSignalPower(f32),
    RfssStatus(SerdeRfssStatus),
    NetworkStatus(SerdeNetworkStatus),
}

#[derive(Copy, Clone, Debug)]
pub enum StateEvent {
    UpdateCtlFreq(u32),
}

pub struct State {
    ctlfreq: AtomicU32,
}

impl Default for State {
    fn default() -> Self {
        State {
            ctlfreq: AtomicU32::new(std::u32::MAX),
        }
    }
}

impl State {
    fn update(&self, e: StateEvent) {
        use self::StateEvent::*;

        match e {
            UpdateCtlFreq(f) => self.ctlfreq.store(f, Ordering::Relaxed),
        }
    }
}

pub struct StateTask {
    events: Receiver<HubEvent>,
    state: Arc<State>,
}

impl StateTask {
    pub fn new(events: Receiver<HubEvent>) -> Self {
        StateTask {
            events: events,
            state: Arc::new(State::default()),
        }
    }

    pub fn get_ref(&self) -> Arc<State> { self.state.clone() }

    pub fn run(&mut self) {
        loop {
            let e = self.events.recv().expect("unable to receive event");

            if let HubEvent::State(sm) = e {
                self.state.update(sm);
            }
        }
    }
}

pub struct SocketTask {
    tcp: TcpListener,
    conns: chan::Sender<TcpStream>,
}

impl SocketTask {
    pub fn new(tcp: TcpListener, conns: chan::Sender<TcpStream>) -> Self {
        SocketTask {
            tcp: tcp,
            conns: conns,
        }
    }

    pub fn run(&mut self) {
        loop {
            let (stream, _) = self.tcp.accept().expect("unable to accept");
            self.conns.send(stream);
        }
    }
}

pub struct HttpTask {
    recv: Sender<ReceiverEvent>,
    state: Arc<State>,
    events: chan::Receiver<TcpStream>,
    stream: Arc<Broadcast<HubEvent>>,
}

impl HttpTask {
    pub fn new(recv: Sender<ReceiverEvent>, state: Arc<State>,
               events: chan::Receiver<TcpStream>, stream: Arc<Broadcast<HubEvent>>)
        -> Self
    {
        HttpTask {
            recv: recv,
            state: state,
            events: events,
            stream: stream,
        }
    }

    pub fn run(&mut self) {
        loop {
            let stream = self.events.recv().expect("unable to receive http stream");
            self.wrap_err(stream);
        }
    }

    fn wrap_err(&mut self, mut s: TcpStream) {
        match self.handle(&mut s) {
            Ok(()) => {},
            Err(e) => { http::send_status(&mut s, e).is_ok(); }
        }
    }

    fn handle(&mut self, s: &mut TcpStream) -> HttpResult<()> {
        let mut buf = [0; 8192];

        let mut req = HttpRequest::new(s, &mut buf[..])?;
        let (ver, method, route) = req.route()?;

        if ver != HttpVersion::from_parts(1, 1) {
            return Err(StatusCode::NotImplemented);
        }

        match (method, route) {
            (Method::Get, Route::Subscribe) => {
                if let Ok(s) = req.into_stream().try_clone() {
                    StreamTask::new(self.stream.connect(), s).run().is_ok();
                    Ok(())
                } else {
                    Err(StatusCode::InternalServerError)
                }
            },
            (Method::Get, Route::CtlFreq) => {
                http::send_json(req.into_stream(), SerdeCtlFreq {
                    ctlfreq: self.state.ctlfreq.load(Ordering::Relaxed),
                }).is_ok();

                Ok(())
            },
            (Method::Post, Route::CtlFreq) => {
                let msg: SerdeCtlFreq = req.read_json()?;

                // TODO: verify frequency range.

                try!(self.recv.send(ReceiverEvent::SetControlFreq(msg.ctlfreq))
                    .map_err(|_| StatusCode::InternalServerError));

                http::send_status(req.into_stream(), StatusCode::Ok).is_ok();

                Ok(())
            },
            _ => Err(StatusCode::MethodNotAllowed),
        }
    }
}

pub struct StreamTask {
    events: Receiver<HubEvent>,
    stream: TcpStream,
}

impl StreamTask {
    pub fn new(events: Receiver<HubEvent>, stream: TcpStream) -> Self {
        StreamTask {
            events: events,
            stream: stream,
        }
    }

    pub fn run(&mut self) -> std::io::Result<()> {
        {
            let mut h = HeaderLines::new(&mut self.stream);
            try!(http::send_head(&mut h, StatusCode::Ok));
            try!(write!(h.line(), "Content-Type: text/event-stream"));
        }

        loop {
            let e = self.events.recv().expect("unable to receive stream event");

            try!(self.handle_event(e)
                 .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other)));
        }
    }

    fn handle_event(&mut self, e: HubEvent) -> serde_json::Result<()> {
        use serde_json::to_writer as write_json;

        use self::HubEvent::*;
        use self::StateEvent::*;

        let mut msg = SseMessage::new(&mut self.stream);

        match e {
            State(UpdateCtlFreq(f)) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "ctlFreq",
                payload: f
            }),

            UpdateCurFreq(f) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "curFreq",
                payload: f,
            }),

            UpdateTalkGroup(tg) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "talkGroup",
                payload: tg,
            }),

            UpdateSignalPower(p) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "sigPower",
                payload: p,
            }),

            RfssStatus(s) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "rfssStatus",
                payload: s,
            }),

            NetworkStatus(s) => write_json(&mut msg.data()?, &SerdeEvent {
                event: "networkStatus",
                payload: s,
            }),
        }
    }
}

pub struct Broadcast<T: Clone> {
    input: Receiver<T>,
    outputs: Mutex<Vec<Sender<T>>>,
}

impl<T: Clone> Broadcast<T> {
    pub fn new(recv: Receiver<T>) -> Self {
        Broadcast {
            input: recv,
            outputs: Mutex::new(vec![]),
        }
    }

    pub fn run(&self) -> Result<(), ()> {
        loop {
            let event = self.input.recv().map_err(|_| ())?;

            let mut outputs = self.outputs.lock().expect("unable to lock outputs");

            let remove = outputs.iter().enumerate().fold(None, |r, (i, o)| {
                match o.send(event.clone()) {
                    Ok(()) => r.or(None),
                    Err(_) => r.or(Some(i)),
                }
            });

            if let Some(idx) = remove {
                outputs.swap_remove(idx);
            }
        }
    }

    pub fn connect(&self) -> Receiver<T> {
        let (tx, rx) = channel();

        let mut outputs = self.outputs.lock().expect("unable to lock outputs");
        outputs.push(tx);

        rx
    }
}

unsafe impl<T: Clone> Sync for Broadcast<T> {}

#[derive(Deserialize, Serialize)]
struct SerdeCtlFreq {
    ctlfreq: u32,
}

#[derive(Serialize)]
struct SerdeEvent<T: Serialize> {
    event: &'static str,
    payload: T,
}

#[derive(Serialize, Clone, Copy)]
pub struct SerdeRfssStatus {
    area: u8,
    system: u16,
    rfss: u8,
    site: u8,
}

impl SerdeRfssStatus {
    pub fn new(s: &RfssStatusBroadcast) -> Self {
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
    pub fn new(s: &NetworkStatusBroadcast) -> Self {
        SerdeNetworkStatus {
            area: s.area(),
            wacn: s.wacn(),
            system: s.system(),
        }
    }
}
