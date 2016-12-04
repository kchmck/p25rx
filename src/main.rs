extern crate clap;
extern crate collect_slice;
extern crate demod_fm;
extern crate fnv;
extern crate imbe;
extern crate libc;
extern crate map_in_place;
extern crate num;
extern crate p25;
extern crate p25_filts;
extern crate pi25_cfg;
extern crate pool;
extern crate prctl;
extern crate rtlsdr;
extern crate rtlsdr_iq;
extern crate sigpower;
extern crate static_decimate;
extern crate throttle;
extern crate xdg_basedir;

#[macro_use]
extern crate static_fir;

use clap::{Arg, App};
use pi25_cfg::sites::parse_sites;
use rtlsdr::TunerGains;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read};
use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use xdg_basedir::dirs;

mod audio;
mod demod;
mod recv;
mod sdr;
mod ui;
mod consts;

use audio::Audio;
use consts::SDR_SAMPLE_RATE;
use demod::Demod;
use recv::P25Receiver;
use sdr::{BlockReader, Controller};
use ui::MainApp;

fn main() {
    let args = App::new("pi25")
        .arg(Arg::with_name("ppm")
             .short("p")
             .help("ppm frequency adjustment")
             .value_name("PPM"))
        .arg(Arg::with_name("audio")
             .short("a")
             .help("file/fifo for audio samples (f32le/8kHz/mono)")
             .required(true)
             .value_name("FILE"))
        .arg(Arg::with_name("gain")
             .short("g")
             .help("tuner gain (use -g list to see all options)")
             .required(true)
             .value_name("GAIN"))
        .arg(Arg::with_name("write")
             .short("w")
             .help("write baseband samples to FILE")
             .value_name("FILE"))
        .get_matches();

    let ppm: i32 = match args.value_of("ppm") {
        Some(s) => s.parse().expect("invalid ppm"),
        None => 0,
    };

    let sites = {
        let mut file = match args.value_of("conf") {
            Some(path) => File::open(path),
            None => {
                let mut conf = dirs::get_config_home().unwrap();
                conf.push("pi25/p25.toml");

                File::open(conf)
            },
        }.expect("unable to open config file");

        let mut toml = String::new();
        file.read_to_string(&mut toml).unwrap();

        Arc::new(parse_sites(&toml).expect("error parsing config file"))
    };

    if sites.len() == 0 {
        println!("no sites configured");
        return;
    }

    let site: usize = match args.value_of("site") {
        Some(name) => sites.iter().position(|s| s.name == name)
            .expect("invalid site name"),
        None => 0,
    };

    let audio_file = BufWriter::new(
        OpenOptions::new()
            .write(true)
            .open(args.value_of("audio").unwrap())
            .expect("unable to open audio output file")
    );

    let samples_file = args.value_of("write")
        .map(|path| File::create(path).expect("unable to open baseband file"));

    let (tx_ui_ev, rx_ui_ev) = channel();
    let (tx_ctl_ev, rx_ctl_ev) = channel();
    let (tx_recv_ev, rx_recv_ev) = channel();
    let (tx_sdr_samp, rx_sdr_samp) = channel();
    let (tx_aud_ev, rx_aud_ev) = channel();

    let mut app = MainApp::new(sites.clone(), site, rx_ui_ev,
        tx_ctl_ev.clone(), tx_recv_ev.clone());
    let mut audio = Audio::new(audio_file, rx_aud_ev);
    let mut receiver = P25Receiver::new(samples_file, rx_recv_ev, tx_ui_ev.clone(),
        tx_ctl_ev.clone(), tx_aud_ev.clone());

    let (mut control, reader) = rtlsdr::open(0).expect("unable to open rtlsdr");

    match args.value_of("gain").unwrap() {
        "list" => {
            let mut gains = TunerGains::default();
            let ngains = control.get_tuner_gains(&mut gains);

            for g in &gains[..ngains] {
                println!("{}", g);
            }

            println!("auto");

            return;
        },
        "auto" => assert!(control.enable_agc()),
        s => assert!(control.set_tuner_gain(s.parse().expect("invalid gain"))),
    }

    // librtlsdr doesn't like zero ppm.
    if ppm != 0 {
        assert!(control.set_ppm(ppm));
    }

    assert!(control.set_sample_rate(SDR_SAMPLE_RATE));
    assert!(control.reset_buf());

    let mut controller = Controller::new(control, rx_ctl_ev);
    let mut radio = BlockReader::new(tx_sdr_samp);
    let mut demod = Demod::new(rx_sdr_samp, tx_ui_ev.clone(), tx_recv_ev.clone());

    thread::spawn(move || {
        prctl::set_name("controller").unwrap();
        controller.run()
    });

    thread::spawn(move || {
        prctl::set_name("reader").unwrap();
        radio.run(reader);
    });

    thread::spawn(move || {
        prctl::set_name("demod").unwrap();
        demod.run();
    });

    thread::spawn(move || {
        prctl::set_name("receiver").unwrap();
        receiver.run();
    });

    thread::spawn(move || {
        prctl::set_name("audio").unwrap();
        audio.run();
    });

    thread::spawn(move || {
        prctl::set_name("ui").unwrap();
        app.run();
    }).join().unwrap();
}
