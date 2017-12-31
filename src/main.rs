#![feature(try_from)]

#[macro_use]
extern crate serde_derive;

extern crate arrayvec;
extern crate chan;
extern crate chrono;
extern crate clap;
extern crate collect_slice;
extern crate crossbeam;
extern crate demod_fm;
extern crate fnv;
extern crate imbe;
extern crate libc;
extern crate map_in_place;
extern crate mio;
extern crate moving_avg;
extern crate num;
extern crate p25;
extern crate p25_filts;
extern crate pool;
extern crate prctl;
extern crate rtlsdr;
extern crate rtlsdr_iq;
extern crate serde;
extern crate serde_json;
extern crate slice_cast;
extern crate static_decimate;
extern crate static_fir;
extern crate throttle;
extern crate uhttp_chunked_write;
extern crate uhttp_json_api;
extern crate uhttp_method;
extern crate uhttp_response_header;
extern crate uhttp_sse;
extern crate uhttp_status;
extern crate uhttp_uri;
extern crate uhttp_version;

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::mpsc::channel;

use clap::{Arg, App};
use rtlsdr::TunerGains;

mod audio;
mod consts;
mod demod;
mod http;
mod hub;
mod recv;
mod sdr;

use audio::{AudioOutput, AudioTask};
use consts::SDR_SAMPLE_RATE;
use demod::DemodTask;
use hub::HubTask;
use recv::{RecvTask, ReplayReceiver};
use sdr::{ReadTask, ControlTask};

fn main() {
    let args = App::new("p25rx")
        .arg(Arg::with_name("ppm")
             .short("p")
             .help("ppm frequency adjustment")
             .value_name("PPM"))
        .arg(Arg::with_name("audio")
             .short("a")
             .help("file/fifo for audio samples (f32le/8kHz/mono)")
             .value_name("FILE"))
        .arg(Arg::with_name("gain")
             .short("g")
             .help("tuner gain (use -g list to see all options)")
             .value_name("GAIN"))
        .arg(Arg::with_name("replay")
             .short("r")
             .help("replay from baseband samples in FILE")
             .value_name("FILE"))
        .arg(Arg::with_name("write")
             .short("w")
             .help("write baseband samples to FILE (f32le/48kHz/mono)")
             .value_name("FILE"))
        .arg(Arg::with_name("freq")
             .short("f")
             .help("frequency for initial control channel (Hz)")
             .value_name("FREQ"))
        .arg(Arg::with_name("device")
             .short("d")
             .help("rtlsdr device index (use -d list to show all)")
             .value_name("INDEX"))
        .arg(Arg::with_name("bind")
             .short("b")
             .help("HTTP socket bind address (default: 0.0.0.0:8025)")
             .value_name("BIND"))
        .get_matches();

    let audio_out = || {
        AudioOutput::new(BufWriter::new(
            OpenOptions::new()
                .write(true)
                .open(args.value_of("audio").expect("-a option is required"))
                .expect("unable to open audio output file")
        ))
    };

    if let Some(path) = args.value_of("replay") {
        let mut stream = File::open(path).expect("unable to open replay file");
        let mut recv = ReplayReceiver::new(audio_out());

        recv.replay(&mut stream);

        return;
    }

    let ppm: i32 = match args.value_of("ppm") {
        Some(s) => s.parse().expect("invalid ppm"),
        None => 0,
    };

    let samples_file = args.value_of("write")
        .map(|path| File::create(path).expect("unable to open baseband file"));

    let dev: u32 = match args.value_of("device") {
        Some("list") => {
            for (idx, name) in rtlsdr::devices().enumerate() {
                println!("{}: {}", idx, name.to_str().unwrap());
            }

            return;
        },
        Some(s) => s.parse().expect("invalid device index"),
        None => 0,
    };

    let (mut control, reader) = rtlsdr::open(dev).expect("unable to open rtlsdr");

    match args.value_of("gain").expect("-g option is required") {
        "list" => {
            let mut gains = TunerGains::default();

            for g in control.tuner_gains(&mut gains) {
                println!("{}", g);
            }

            println!("auto");

            return;
        },
        "auto" => control.enable_agc().expect("unable to enable agc"),
        s => control.set_tuner_gain(s.parse().expect("invalid gain"))
                .expect("unable to set gain")
    }

    control.set_ppm(ppm).expect("unable to set ppm");
    control.set_sample_rate(SDR_SAMPLE_RATE).expect("unable to set sample rate");

    let freq: u32 = args.value_of("freq").expect("-f option is required")
        .parse().expect("invalid frequency");

    let addr = args.value_of("bind").unwrap_or("0.0.0.0:8025").parse()
        .expect("unable to bind tcp socket");

    let (tx_ctl, rx_ctl) = channel();
    let (tx_recv, rx_recv) = channel();
    let (tx_read, rx_read) = channel();
    let (tx_audio, rx_audio) = channel();
    let (tx_hub, rx_hub) = mio::channel::channel();

    let mut hub = HubTask::new(rx_hub, tx_recv.clone(), &addr)
        .expect("unable to start hub");
    let mut control = ControlTask::new(control, rx_ctl);
    let mut read = ReadTask::new(tx_read);
    let mut demod = DemodTask::new(rx_read, tx_hub.clone(), tx_recv.clone());
    let mut recv = RecvTask::new(freq, rx_recv, tx_hub.clone(),
        tx_ctl.clone(), tx_audio.clone());
    let mut audio = AudioTask::new(audio_out(), rx_audio);

    crossbeam::scope(|scope| {
        scope.spawn(move || {
            prctl::set_name("hub").unwrap();
            hub.run();
        });

        scope.spawn(move || {
            prctl::set_name("controller").unwrap();
            control.run()
        });

        scope.spawn(move || {
            prctl::set_name("reader").unwrap();
            read.run(reader);
        });

        scope.spawn(move || {
            prctl::set_name("demod").unwrap();
            demod.run();
        });

        scope.spawn(move || {
            prctl::set_name("receiver").unwrap();

            if let Some(mut f) = samples_file {
                recv.run(|samples| {
                    // This is safe because it's converting N 32-bit words to 4N 8-bit
                    // words.
                    f.write_all(unsafe {
                        slice_cast::cast(samples)
                    }).expect("unable to write baseband");
                })
            } else {
                recv.run(|_| {})
            }
        });

        scope.spawn(move || {
            prctl::set_name("audio").unwrap();
            audio.run();
        });
    });
}
