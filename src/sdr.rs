extern crate cfg;
extern crate collect_slice;
extern crate imbe;
extern crate libc;
extern crate map_in_place;
extern crate num;
extern crate p25;
extern crate pool;
extern crate prctl;
extern crate rtlsdr;
extern crate sigpower;
extern crate ui;
extern crate xdg_basedir;

#[macro_use]
extern crate dsp;

use std::cmp::{min, max};
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Write};
use std::process::Command;
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender, Receiver};
use std::thread;

use cfg::channels::P25Channels;
use cfg::talkgroups::{parse_talkgroups, TalkGroups};
use collect_slice::CollectSlice;
use dsp::decim::{DecimationFactor, Decimator};
use dsp::fir::FIRFilter;
use dsp::fm::FMDemod;
use imbe::decoder::{IMBEDecoder, CAIFrame};
use map_in_place::MapInPlace;
use num::complex::Complex32;
use num::traits::Zero;
use p25::error::P25Error;
use p25::filters::ReceiveFilter;
use p25::nid::NetworkID;
use p25::receiver::{ReceiverEvent, DataUnitReceiver};
use p25::status::StreamSymbol;
use p25::trunking::decode::TalkGroup;
use p25::trunking::tsbk::{self, TSBKFields, TSBKReceiver};
use p25::voice::control::LinkControlFields;
use p25::voice::crypto::CryptoControlFields;
use p25::voice::frame::VoiceFrame;
use p25::voice::header::VoiceHeaderFields;
use pool::{Pool, Checkout};
use rtlsdr::{Control, Reader, TunerGains};
use sigpower::power;
use sigpower::smeter::SignalLevel;
use ui::button::Button;
use ui::lcd::LCD;
use ui::rotary::{RotaryDecoder, Rotation};
use xdg_basedir::dirs;

use p25::voice::{
    FrameGroupEvent,
    VoiceCCFrameGroupReceiver,
    VoiceHeaderReceiver,
    VoiceLCFrameGroupReceiver,
    VoiceLCTerminatorReceiver,
};

mod filters;
mod iq;

use filters::{SecondDecimFIR, BandpassFIR};

const BUF_COUNT: usize = 16;
const BUF_SIZE: usize = 32768;

const MIN_VOL: usize = 80;
const MAX_VOL: usize = 100;
const VOL_STEP: usize = 1;

const SAMPLE_RATE: u32 = 240_000;
const BASEBAND_SAMPLE_RATE: u32 = 48000;
const FM_DEV: u32 = 5000;

const IMBE_FILE: &'static str = "imbe.fifo";

pub struct Decimate5;

impl DecimationFactor for Decimate5 {
    fn factor() -> u32 { 5 }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum UIEvent {
    Rotation(Rotation),
    ButtonPress,
    SetTalkGroup(TalkGroup),
    SetSignalLevel(SignalLevel),
    SetFreq(u32),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum View {
    Main,
    Volume,
    Settings,
    SettingGain,
    AdjustGain,
    SettingExit,
    Poweroff,
}

impl Default for View {
    fn default() -> Self { View::Main }
}

impl View {
    pub fn next(self) -> View {
        use self::View::*;

        match self {
            Main => Settings,
            Settings => Poweroff,
            SettingGain => SettingExit,
            SettingExit => SettingGain,
            Poweroff => Main,
            _ => unreachable!(),
        }
    }

    pub fn prev(self) -> View {
        use self::View::*;

        match self {
            Main => Poweroff,
            Settings => Main,
            SettingGain => SettingExit,
            SettingExit => SettingGain,
            Poweroff => Settings,
            _ => unreachable!(),
        }
    }

    pub fn select(self) -> View {
        use self::View::*;

        match self {
            Main => Volume,
            Volume => Main,
            Settings => SettingGain,
            SettingGain => AdjustGain,
            AdjustGain => SettingGain,
            SettingExit => Settings,
            _ => unreachable!(),
        }
    }
}

struct AppState {
    pub gains: (TunerGains, usize),
    pub view: View,
    pub volume: usize,
    /// Index into gains array.
    pub gain: usize,
    pub talkgroup: TalkGroup,
    pub freq: u32,
    pub signal: SignalLevel,
}

impl AppState {
    pub fn increase_volume(&mut self) {
        self.volume = min(self.volume.saturating_add(VOL_STEP), MAX_VOL);
    }

    pub fn decrease_volume(&mut self) {
        self.volume = max(self.volume.saturating_sub(VOL_STEP), MIN_VOL);
    }

    pub fn increase_gain(&mut self) {
        self.gain = min(self.gain.saturating_add(1), self.gains.1 - 1);
    }

    pub fn decrease_gain(&mut self) {
        self.gain = max(self.gain.saturating_sub(1), 0);
    }
}

struct MainApp {
    lcd: LCD,
    state: AppState,
    talkgroups: TalkGroups,
    events: Receiver<UIEvent>,
    sdr: SyncSender<ControllerEvent>,
}

impl MainApp {
    pub fn new(talkgroups: TalkGroups,
               gains: (TunerGains, usize),
               events: Receiver<UIEvent>,
               sdr: SyncSender<ControllerEvent>)
        -> MainApp
    {
        MainApp {
            lcd: LCD::new(),
            state: AppState {
                gains: gains,
                view: View::default(),
                volume: (MAX_VOL + MIN_VOL) / 2,
                gain: gains.1 - 1,
                talkgroup: TalkGroup::Nobody,
                freq: 0,
                signal: SignalLevel::None,
            },
            talkgroups: talkgroups,
            events: events,
            sdr: sdr,
        }.init()
    }

    fn init(mut self) -> Self {
        self.commit_gain();
        self.commit_volume();

        self.lcd.backlight_on();
        self.lcd.create_char(0, [ 0,  0,  0,  0,  0,  0,  0, 31]);
        self.lcd.create_char(1, [ 0,  0,  0,  0,  0, 31, 31, 31]);
        self.lcd.create_char(2, [ 0,  0,  0, 31, 31, 31, 31, 31]);
        self.lcd.create_char(3, [ 0, 31, 31, 31, 31, 31, 31, 31]);
        self.lcd.create_char(4, [31, 31, 31, 31, 31, 31, 31, 31]);

        self.redraw();

        self
    }

    pub fn redraw(&mut self) {
        let mut top = [b' '; ui::lcd::COLS as usize];
        let mut bot = [b' '; ui::lcd::COLS as usize];

        self.draw(&mut top[..], &mut bot[..]);

        self.lcd.cursor(0, 0);
        self.lcd.message(&mut top[..]);
        self.lcd.cursor(1, 0);
        self.lcd.message(&mut bot[..]);
    }

    fn draw(&self, mut top: &mut [u8], mut bot: &mut [u8]) {
        match self.state.view {
            View::Main => {
                match self.state.talkgroup {
                    TalkGroup::Nobody => write!(top, "(Nobody)"),
                    TalkGroup::Default => write!(top, "(Default)"),
                    TalkGroup::Everbody => write!(top, "(Everybody)"),
                    TalkGroup::Other(tg) => match self.talkgroups.get(&tg) {
                        Some(s) => write!(top, "{}", s),
                        None => write!(top, "(0x{:04X})", tg),
                    },
                }.unwrap();

                let mut cursor = std::io::Cursor::new(bot);
                write!(cursor, "{: <13} S", self.state.freq).unwrap();

                match self.state.signal {
                    SignalLevel::Plus(_) => write!(cursor, "+"),
                    SignalLevel::Level(l) => write!(cursor, "{}", l),
                    SignalLevel::None => write!(cursor, "-"),
                }.unwrap()
            },
            View::Volume => {
                write!(top, "Volume").unwrap();
                self.draw_volume(bot);
            },
            View::Settings => write!(top, "Settings").unwrap(),
            View::SettingGain => self.draw_gain(top, bot, ' '),
            View::AdjustGain => self.draw_gain(top, bot, '\x7e'),
            View::SettingExit => write!(top, "Exit?").unwrap(),
            View::Poweroff => write!(top, "Poweroff?").unwrap(),
        }
    }

    fn draw_gain(&self, mut top: &mut [u8], mut bot: &mut [u8], prefix: char) {
        write!(top, "Tuner Gain").unwrap();
        write!(bot, "{}{: >13.1}dB", prefix,
            self.state.gains.0[self.state.gain] as f32 / 10.0).unwrap();
    }

    fn draw_volume(&self, buf: &mut [u8]) {
        const MAP: [u8; 4] = [b'\x00', b'\x01', b'\x02', b'\x03'];
        const FULL: u8 = b'\x04';

        let bars = 1 + (self.state.volume - MIN_VOL) *
            (ui::lcd::COLS as usize - 1) / (MAX_VOL - MIN_VOL);
        let mut cursor = std::io::Cursor::new(buf);

        cursor.write_all(&MAP[..min(MAP.len(), bars)]).unwrap();

        for _ in MAP.len()..bars as usize {
            cursor.write_all(&[FULL]).unwrap();
        }
    }

    fn commit_volume(&self) {
        assert!(Command::new("amixer")
                        .arg("sset")
                        .arg("PCM")
                        .arg(format!("{}%", self.state.volume))
                        .status()
                        .unwrap()
                        .success());
    }

    fn poweroff(&mut self) {
        self.lcd.clear();
        self.lcd.backlight_off();

        assert!(Command::new("sudo")
                        .arg("systemctl")
                        .arg("poweroff")
                        .status()
                        .unwrap()
                        .success());
    }

    fn commit_gain(&mut self) {
        self.sdr.send(ControllerEvent::SetGain(self.state.gains.0[self.state.gain]))
            .expect("unable to commit gain");
    }

    pub fn run(&mut self) {
        loop {
            match self.events.recv() {
                Ok(event) => {
                    self.handle(event);
                    self.redraw();
                },
                Err(_) => break,
            }
        }
    }

    fn handle(&mut self, event: UIEvent) {
        use self::View::*;

        match event {
            UIEvent::Rotation(Rotation::Clockwise) => match self.state.view {
                Volume => {
                    self.state.increase_volume();
                    self.commit_volume();
                },
                AdjustGain => self.state.increase_gain(),
                _ => self.state.view = self.state.view.next(),
            },
            UIEvent::Rotation(Rotation::CounterClockwise) => match self.state.view {
                Volume => {
                    self.state.decrease_volume();
                    self.commit_volume();
                },
                AdjustGain =>self.state.decrease_gain(),
                _ => self.state.view = self.state.view.prev(),
            },
            UIEvent::ButtonPress => match self.state.view {
                Poweroff => self.poweroff(),
                AdjustGain => {
                    self.commit_gain();
                    self.state.view = self.state.view.select();
                },
                _ => self.state.view = self.state.view.select(),
            },
            UIEvent::SetTalkGroup(tg) => self.state.talkgroup = tg,
            UIEvent::SetSignalLevel(s) => self.state.signal = s,
            UIEvent::SetFreq(freq) =>  self.state.freq = freq,
        }
    }
}

struct Demod {
    decim: Decimator<Decimate5, SecondDecimFIR>,
    bandpass: FIRFilter<BandpassFIR>,
    demod: FMDemod,
    reader: Receiver<Checkout<Vec<u8>>>,
    ui: SyncSender<UIEvent>,
    chan: SyncSender<Checkout<Vec<f32>>>,
}

impl Demod {
    pub fn new(reader: Receiver<Checkout<Vec<u8>>>,
               ui: SyncSender<UIEvent>,
               chan: SyncSender<Checkout<Vec<f32>>>)
        -> Demod
    {
        Demod {
            decim: Decimator::new(),
            bandpass: FIRFilter::new(),
            demod: FMDemod::new(FM_DEV as f32 / BASEBAND_SAMPLE_RATE as f32),
            reader: reader,
            ui: ui,
            chan: chan,
        }
    }

    pub fn run(&mut self) {
        let mut pool = Pool::with_capacity(16, || vec![0.0; BUF_SIZE / 2]);
        let mut samples = vec![Complex32::zero(); BUF_SIZE / 2];

        let mut iter = 0;

        loop {
            let bytes = self.reader.recv().expect("unable to receive sdr samples");

            let pairs = unsafe {
                std::slice::from_raw_parts(bytes.as_ptr() as *const u16, BUF_SIZE / 2)
            };

            unsafe { samples.set_len(BUF_SIZE / 2); }

            pairs.iter()
                 .map(|&s| unsafe { *iq::IQ.get_unchecked(s as usize) })
                 .collect_slice(&mut samples[..]);

            let len = self.decim.decim_in_place(&mut samples[..]);

            // This is safe for the same reason as above.
            unsafe { samples.set_len(len); }

            samples.map_in_place(|&s| self.bandpass.feed(s));

            let level = SignalLevel::from_dbm(
                power::power_dbm(&samples[..], 50.0) - 106.0);

            iter += 1;
            iter %= 16;

            if iter == 0 {
                self.ui.send(UIEvent::SetSignalLevel(level))
                    .expect("unable to send signal level");
            }

            let mut baseband = pool.checkout().expect("unable to allocate baseband");

            // This is safe because each input sample produces exactly one output sample.
            unsafe { baseband.set_len(samples.len()); }

            samples.iter()
                   .map(|&s| self.demod.feed(s))
                   .collect_slice(&mut baseband[..]);

            self.chan.send(baseband).expect("unable to send baseband");
        }
    }
}

struct Radio {
    chan: SyncSender<Checkout<Vec<u8>>>,
}

impl Radio {
    pub fn new(chan: SyncSender<Checkout<Vec<u8>>>) -> Radio {
        Radio {
            chan: chan,
        }
    }

    pub fn run(&mut self, mut reader: Reader) {
        let mut pool = Pool::with_capacity(16, || vec![0; BUF_SIZE]);

        reader.read_async(BUF_COUNT as u32, BUF_SIZE as u32, |bytes| {
            let mut samples = pool.checkout().expect("unable to allocate samples");
            (&mut samples[..]).write(bytes).unwrap();
            self.chan.send(samples).expect("unable to send sdr samples");
        });
    }
}

enum ControllerEvent {
    SetFreq(u32),
    SetGain(i32),
}

struct Controller {
    sdr: Control,
    events: Receiver<ControllerEvent>,
}

impl Controller {
    pub fn new(sdr: Control, events: Receiver<ControllerEvent>) -> Controller {
        Controller {
            sdr: sdr,
            events: events,
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.events.recv() {
                Ok(ControllerEvent::SetFreq(freq)) =>
                    assert!(self.sdr.set_center_freq(freq)),
                Ok(ControllerEvent::SetGain(gain)) =>
                    assert!(self.sdr.set_tuner_gain(gain)),
                Err(_) => break,
            }
        }
    }
}

struct P25Receiver {
    channels: P25Channels,
    demod: Receiver<Checkout<Vec<f32>>>,
    ui: SyncSender<UIEvent>,
    sdr: SyncSender<ControllerEvent>,
    audio: Sender<AudioEvent>,
}

impl P25Receiver {
    pub fn new(channels: P25Channels,
               demod: Receiver<Checkout<Vec<f32>>>,
               ui: SyncSender<UIEvent>,
               sdr: SyncSender<ControllerEvent>,
               audio: Sender<AudioEvent>)
        -> P25Receiver
    {
        P25Receiver {
            channels: channels,
            demod: demod,
            ui: ui,
            sdr: sdr,
            audio: audio,
        }.init()
    }

    fn init(self) -> Self {
        self.set_freq(self.channels.control);
        self
    }

    fn set_freq(&self, freq: u32) {
        self.ui.send(UIEvent::SetFreq(freq))
            .expect("unable to update freq in UI");
        self.sdr.send(ControllerEvent::SetFreq(freq))
            .expect("unable to set freq in sdr");
    }

    pub fn run(&mut self) {
        let mut messages = MessageReceiver::new();

        loop {
            let baseband = self.demod.recv().expect("unable to receive baseband");

            for &s in baseband.iter() {
                messages.feed(s, self);
            }
        }
    }
}

impl P25Handler for P25Receiver {
    fn handle_error(&mut self, _: P25Error) {}
    fn handle_nid(&mut self, _: NetworkID) {}
    fn handle_header(&mut self, _: VoiceHeaderFields) {}
    fn handle_lc(&mut self, _: LinkControlFields) {}
    fn handle_cc(&mut self, _: CryptoControlFields) {}
    fn handle_data_frag(&mut self, _: u32) {}

    fn handle_frame(&mut self, vf: VoiceFrame) {
        self.audio.send(AudioEvent::VoiceFrame(vf))
            .expect("unable to send voice frame");
    }

    fn handle_tsbk(&mut self, tsbk: TSBKFields) {
        use p25::trunking::tsbk::TSBKOpcode::*;

        if tsbk.mfg() != 0 {
            return;
        }

        let print_common = || {
            println!("");
            println!("prot:{} opcode:{:?}", tsbk.protected(), tsbk.opcode());
            println!("mfg:{:08b}", tsbk.mfg());
            println!("crc:{:016b}", tsbk.crc());
            println!("    {:016b}", tsbk.calc_crc());
        };

        match tsbk.opcode().unwrap() {
            UnitCallRequest => {
                let dec = tsbk::UnitCallRequest::new(tsbk);
                print_common();
                println!("src:{:x}", dec.src_id());
                println!("dest:{:x}", dec.dest_unit());
            },
            AltControlBroadcast => {
                // let dec = tsbk::AltControlBroadcast::new(tsbk);
                // let ch1 = dec.channel_a();
                // let ch2 = dec.channel_b();
                // println!("    rfss:{:x}", dec.rfss());
                // println!("    site:{:x}", dec.site());
                // println!("    channel A");
                // println!("      band:{}", ch1.band());
                // println!("      number:{}", ch1.number());
                // println!("      services:{:?}", dec.services_a());
                // println!("    channel B");
                // println!("      band:{}", ch2.band());
                // println!("      number:{}", ch2.number());
                // println!("      services:{:?}", dec.serviced_b());
            },
            AdjacentSiteBroadcast | RFSSStatusBroadcast => {
            //     let dec = tsbk::SiteStatusBroadcast::new(tsbk);
            //     let ch = dec.channel();
            //     let svc = dec.services();
            //     println!("    area:{:x}", dec.area());
            //     println!("    system:{:x}", dec.system());
            //     println!("    rfss:{:x}", dec.rfss());
            //     println!("    site:{:x}", dec.site());
            //     println!("    channel");
            //     println!("      band:{}", ch.band());
            //     println!("      number:{}", ch.number());
            //     println!("    services");
            //     println!("      composite:{}", svc.is_composite());
            //     println!("      updates:{}", svc.has_updates());
            //     println!("      backup:{}", svc.is_backup());
            //     println!("      data:{}", svc.has_data());
            //     println!("      voice:{}", svc.has_voice());
            //     println!("      reg:{}", svc.has_registration());
            //     println!("      auth:{}", svc.has_auth());
            },
            ChannelParamsUpdate => {
                // let dec = tsbk::ChannelParamsUpdate::new(tsbk);
                // let p = dec.params();
                // println!("    tx:{}Hz", p.tx_freq);
                // println!("    rx:{}Hz", p.rx_freq);
                // println!("    bw:{}Hz", p.bandwidth);
            },
            NetworkStatusBroadcast => {
            //     let dec = tsbk::NetworkStatusBroadcast::new(tsbk);
            //     let ch = dec.channel();
            //     let svc = dec.services();
            //     println!("    area:{:x}", dec.area());
            //     println!("    wacn:{:x}", dec.wacn());
            //     println!("    system:{:x}", dec.system());
            //     println!("    channel");
            //     println!("      band:{}", ch.band());
            //     println!("      number:{}", ch.number());
            //     println!("    services");
            //     println!("      composite:{}", svc.is_composite());
            //     println!("      updates:{}", svc.has_updates());
            //     println!("      backup:{}", svc.is_backup());
            //     println!("      data:{}", svc.has_data());
            //     println!("      voice:{}", svc.has_voice());
            //     println!("      reg:{}", svc.has_registration());
            //     println!("      auth:{}", svc.has_auth());
            },
            GroupVoiceUpdate => {
                let dec = tsbk::GroupVoiceUpdate::new(tsbk);
                let ch1 = dec.channel_a();

                if dec.talk_group_a() == TalkGroup::Other(0xCB68) {
                    return;
                }

                self.ui.send(UIEvent::SetTalkGroup(dec.talk_group_a()))
                    .expect("unable to send talkgroup");

                match ch1.number() {
                    1796 => self.set_freq(773_231_250),
                    1368 => self.set_freq(770_556_250),
                    1280 => self.set_freq(773_231_250),
                    1160 => self.set_freq(769_256_250),
                    _ => {},
                }

                let ch2 = dec.channel_b();
                // print_common();
                println!("talkgroup 1:{:?}", dec.talk_group_a());
                // println!("channel 1");
                // println!("  band:{}", ch1.band());
                println!("  number:{}", ch1.number());
                println!("talkgroup 2:{:?}", dec.talk_group_b());
                // println!("channel 2");
                // println!("  band:{}", ch2.band());
                println!("  number:{}", ch2.number());
            },
            GroupVoiceGrant => {
                // let dec = tsbk::GroupVoiceGrant::new(tsbk);
                // print_common();
                // println!("talkgroup:{:?}", dec.talk_group());
                // println!("unit:0x{:X}", dec.src_unit());
            },
            // GroupAffiliationQuery
            // LocRegistrationResponse
            // GroupAffiliationResponse
            // UnitRegistrationResponse
            // DeregistrationAck
            // AckResponse
            Reserved => {},
            _ => {
                // println!("    NOT HANDLED {:?}", tsbk.opcode());
            },
        }
    }

    fn handle_term(&mut self) {
        self.set_freq(self.channels.control);
        self.audio.send(AudioEvent::EndTransmission)
            .expect("unable to send end of transmission");
    }
}

enum AudioEvent {
    VoiceFrame(VoiceFrame),
    EndTransmission,
}

struct Audio {
    imbe: IMBEDecoder,
    out: BufWriter<File>,
    queue: Receiver<AudioEvent>,
}

impl Audio {
    pub fn new(queue: Receiver<AudioEvent>) -> Audio {
        Audio {
            imbe: IMBEDecoder::new(),
            out: BufWriter::new(OpenOptions::new().write(true).open(IMBE_FILE).unwrap()),
            queue: queue,
        }
    }

    fn handle(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::VoiceFrame(vf) => {
                let frame = CAIFrame::new(vf.chunks, vf.errors);

                let mut samples = [0.0; imbe::consts::SAMPLES];
                self.imbe.decode(frame, &mut samples);
                samples.map_in_place(|&s| s / 8192.0);

                self.out.write_all(unsafe {
                    std::slice::from_raw_parts(samples.as_ptr() as *const u8,
                        samples.len() * 4)
                }).unwrap();
            },
            AudioEvent::EndTransmission => self.out.flush().unwrap(),
        }
    }

    pub fn run(&mut self) {
        loop {
            match self.queue.recv() {
                Ok(event) => self.handle(event),
                Err(_) => {},
            }
        }
    }
}

fn main() {
    let (mut control, reader) = rtlsdr::open(0).expect("unable to open rtlsdr");

    let gains = {
        let mut gains = TunerGains::default();
        let ngains = control.get_tuner_gains(&mut gains);
        (gains, ngains)
    };

    assert!(control.set_sample_rate(SAMPLE_RATE));
    assert!(control.set_ppm(-2));
    assert!(control.reset_buf());

    let mut conf = dirs::get_config_home().unwrap();
    conf.push("recv-p25");

    let talkgroups = {
        let mut conf = conf.clone();
        conf.push("talkgroups.csv");

        parse_talkgroups(File::open(conf).unwrap())
    };

    let channels = {
        let mut conf = conf.clone();
        conf.push("p25.toml");

        let mut toml = String::new();
        File::open(conf).unwrap().read_to_string(&mut toml).unwrap();

        P25Channels::new(&toml).unwrap()
    };

    let (tx_ui_ev, rx_ui_ev) = sync_channel(64);
    let (tx_ctl_ev, rx_ctl_ev) = sync_channel(16);

    let (tx_sdr_samp, rx_sdr_samp) = sync_channel(64);
    let (tx_demod_samp, rx_demod_samp) = sync_channel(64);
    let (tx_aud_samp, rx_aud_samp) = channel();

    let mut app = MainApp::new(talkgroups, gains, rx_ui_ev, tx_ctl_ev.clone());
    let mut controller = Controller::new(control, rx_ctl_ev);
    let mut radio = Radio::new(tx_sdr_samp);
    let mut demod = Demod::new(rx_sdr_samp, tx_ui_ev.clone(), tx_demod_samp);
    let mut audio = Audio::new(rx_aud_samp);
    let mut receiver = P25Receiver::new(channels, rx_demod_samp, tx_ui_ev.clone(),
        tx_ctl_ev.clone(), tx_aud_samp.clone());

    let mut rotary = RotaryDecoder::new();
    let tx_rotary = tx_ui_ev.clone();

    let mut button = Button::new();
    let tx_button = tx_ui_ev.clone();

    thread::spawn(move || {
        prctl::set_name("controller").unwrap();
        controller.run()
    });

    thread::spawn(move || {
        set_affinity(0);
        prctl::set_name("reader").unwrap();
        radio.run(reader);
    });

    thread::spawn(move || {
        set_affinity(1);
        prctl::set_name("demod").unwrap();
        demod.run();
    });

    thread::spawn(move || {
        prctl::set_name("audio").unwrap();
        audio.run();
    });

    thread::spawn(move || {
        set_affinity(2);
        prctl::set_name("receiver").unwrap();
        receiver.run();
    });

    thread::spawn(move || {
        prctl::set_name("rotary").unwrap();

        rotary.run(|rot| {
            tx_rotary.send(UIEvent::Rotation(rot))
                .expect("unable to send rotary event");
        });
    });

    thread::spawn(move || {
        prctl::set_name("button").unwrap();

        button.run(|| {
            tx_button.send(UIEvent::ButtonPress)
                .expect("unable to send button press event");
        });
    });

    thread::spawn(move || {
        prctl::set_name("ui").unwrap();
        app.run();
    }).join().unwrap();
}

pub trait P25Handler {
    fn handle_error(&mut self, err: P25Error);
    fn handle_nid(&mut self, nid: NetworkID);
    fn handle_header(&mut self, header: VoiceHeaderFields);
    fn handle_frame(&mut self, frame: VoiceFrame);
    fn handle_lc(&mut self, lc: LinkControlFields);
    fn handle_cc(&mut self, cc: CryptoControlFields);
    fn handle_data_frag(&mut self, data: u32);
    fn handle_tsbk(&mut self, tsbk: TSBKFields);
    fn handle_term(&mut self);
}

pub struct MessageReceiver {
    recv: DataUnitReceiver,
    filt: ReceiveFilter,
    state: MessageReceiverState,
}

impl MessageReceiver {
    pub fn new() -> MessageReceiver {
        MessageReceiver {
            recv: DataUnitReceiver::new(),
            filt: ReceiveFilter::new(),
            state: MessageReceiverState::Idle,
        }
    }

    pub fn feed<H: P25Handler>(&mut self, s: f32, handler: &mut H) {
        use self::MessageReceiverState::*;
        use p25::nid::DataUnit::*;

        let event = match self.recv.feed(self.filt.feed(s)) {
            Some(Ok(event)) => event,
            Some(Err(err)) => {
                handler.handle_error(err);
                self.recv.resync();

                return;
            },
            None => return,
        };

        let dibit = match event {
            ReceiverEvent::NetworkID(nid) => {
                self.state = match nid.data_unit() {
                    VoiceHeader =>
                        DecodeHeader(VoiceHeaderReceiver::new()),
                    VoiceSimpleTerminator => {
                        handler.handle_term();
                        self.recv.flush_pads();
                        Idle
                    },
                    VoiceLCTerminator =>
                        DecodeLCTerminator(VoiceLCTerminatorReceiver::new()),
                    VoiceLCFrameGroup =>
                        DecodeLCFrameGroup(VoiceLCFrameGroupReceiver::new()),
                    VoiceCCFrameGroup =>
                        DecodeCCFrameGroup(VoiceCCFrameGroupReceiver::new()),
                    TrunkingSignaling =>
                        DecodeTSBK(TSBKReceiver::new()),
                    DataPacket => {
                        self.recv.resync();
                        Idle
                    },
                };

                handler.handle_nid(nid);

                return;
            },
            ReceiverEvent::Symbol(StreamSymbol::Status(_)) => return,
            ReceiverEvent::Symbol(StreamSymbol::Data(dibit)) => dibit,
        };

        match self.state {
            DecodeHeader(ref mut head) => match head.feed(dibit) {
                Some(Ok(h)) => {
                    handler.handle_header(h);
                    self.recv.flush_pads();
                },
                Some(Err(err)) => {
                    handler.handle_error(err);
                    self.recv.resync();
                },
                None => {},
            },
            DecodeLCFrameGroup(ref mut fg) => match fg.feed(dibit) {
                Some(Ok(event)) => match event {
                    FrameGroupEvent::VoiceFrame(vf) => {
                        handler.handle_frame(vf);

                        if fg.done() {
                            self.recv.flush_pads();
                        }
                    },
                    FrameGroupEvent::Extra(lc) => handler.handle_lc(lc),
                    FrameGroupEvent::DataFragment(data) => handler.handle_data_frag(data),
                },
                Some(Err(err)) => {
                    handler.handle_error(err);
                    self.recv.resync();
                },
                None => {},
            },
            DecodeCCFrameGroup(ref mut fg) => match fg.feed(dibit) {
                Some(Ok(event)) => match event {
                    FrameGroupEvent::VoiceFrame(vf) => {
                        handler.handle_frame(vf);

                        if fg.done() {
                            self.recv.flush_pads();
                        }
                    },
                    FrameGroupEvent::Extra(cc) => handler.handle_cc(cc),
                    FrameGroupEvent::DataFragment(data) => handler.handle_data_frag(data),
                },
                Some(Err(err)) => {
                    handler.handle_error(err);
                    self.recv.resync();
                },
                None => {},
            },
            DecodeLCTerminator(ref mut term) => match term.feed(dibit) {
                Some(Ok(lc)) => {
                    handler.handle_lc(lc);
                    handler.handle_term();
                    self.recv.flush_pads();
                },
                Some(Err(err)) => {
                    handler.handle_error(err);
                    self.recv.resync();
                },
                None => {},
            },
            DecodeTSBK(ref mut dec) => match dec.feed(dibit) {
                Some(Ok(tsbk)) => {
                    handler.handle_tsbk(tsbk);

                    if tsbk.is_tail() {
                        self.recv.flush_pads();
                    }
                },
                Some(Err(err)) => {
                    handler.handle_error(err);
                    self.recv.resync();
                },
                None => {},
            },
            Idle => {},
        }
    }
}

enum MessageReceiverState {
    Idle,
    DecodeHeader(VoiceHeaderReceiver),
    DecodeLCFrameGroup(VoiceLCFrameGroupReceiver),
    DecodeCCFrameGroup(VoiceCCFrameGroupReceiver),
    DecodeLCTerminator(VoiceLCTerminatorReceiver),
    DecodeTSBK(TSBKReceiver),
}

fn set_affinity(cpu: usize) {
    unsafe {
        let mut cpus = std::mem::zeroed();

        libc::CPU_ZERO(&mut cpus);
        libc::CPU_SET(cpu, &mut cpus);

        assert!(libc::sched_setaffinity(0, std::mem::size_of_val(&cpus), &cpus) == 0);
    }
}
