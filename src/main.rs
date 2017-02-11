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
extern crate pool;
extern crate prctl;
extern crate rtlsdr;
extern crate rtlsdr_iq;
extern crate sigpower;
extern crate static_decimate;
extern crate throttle;

#[macro_use]
extern crate static_fir;

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::sync::mpsc::channel;
use std::thread;

use clap::{Arg, App};
use rtlsdr::TunerGains;

mod audio;
mod demod;
mod recv;
mod sdr;
mod ui;
mod consts;

use audio::{AudioOutput, AudioEvents};
use consts::SDR_SAMPLE_RATE;
use demod::Demod;
use recv::{P25Receiver, ReplayReceiver};
use sdr::{BlockReader, Controller};
use ui::MainApp;

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
             .help("write baseband samples to FILE")
             .value_name("FILE"))
        .arg(Arg::with_name("freq")
             .short("f")
             .help("frequency for initial control channel (Hz)")
             .value_name("FREQ"))
        .get_matches();

    let get_audio_out = || {
        AudioOutput::new(BufWriter::new(
            OpenOptions::new()
                .write(true)
                .open(args.value_of("audio").expect("-a option is required"))
                .expect("unable to open audio output file")
        ))
    };

    if let Some(path) = args.value_of("replay") {
        let mut stream = File::open(path).expect("unable to open replay file");
        let mut recv = ReplayReceiver::new(get_audio_out());

        recv.replay(&mut stream);

        return;
    }

    let ppm: i32 = match args.value_of("ppm") {
        Some(s) => s.parse().expect("invalid ppm"),
        None => 0,
    };

    let samples_file = args.value_of("write")
        .map(|path| File::create(path).expect("unable to open baseband file"));

    let (mut control, reader) = rtlsdr::open(0).expect("unable to open rtlsdr");

    match args.value_of("gain").expect("-g option is required") {
        "list" => {
            let mut gains = TunerGains::default();

            for g in control.tuner_gains(&mut gains) {
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

    let freq: u32 = args.value_of("freq").expect("-f option is required")
        .parse().expect("invalid frequency");

    let (tx_ui_ev, rx_ui_ev) = channel();
    let (tx_ctl_ev, rx_ctl_ev) = channel();
    let (tx_recv_ev, rx_recv_ev) = channel();
    let (tx_sdr_samp, rx_sdr_samp) = channel();
    let (tx_aud_ev, rx_aud_ev) = channel();

    let mut app = MainApp::new(rx_ui_ev, tx_recv_ev.clone());
    let mut audio = AudioEvents::new(get_audio_out(), rx_aud_ev);
    let mut receiver = P25Receiver::new(freq, rx_recv_ev, tx_ui_ev.clone(),
        tx_ctl_ev.clone(), tx_aud_ev.clone());

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

        if let Some(mut f) = samples_file {
            receiver.run(|samples| {
                f.write_all(unsafe {
                    std::slice::from_raw_parts(
                        samples.as_ptr() as *const u8,
                        samples.len() * std::mem::size_of::<f32>()
                    )
                }).expect("unable to write baseband");
            })
        } else {
            receiver.run(|_| {})
        }
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
