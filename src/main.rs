extern crate pi25_cfg;
extern crate collect_slice;
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
extern crate throttle;
extern crate xdg_basedir;
extern crate static_decimate;
extern crate demod_fm;

#[macro_use]
extern crate static_fir;

use pi25_cfg::sites::parse_sites;
use pi25_cfg::talkgroups::parse_talkgroups;
use std::fs::File;
use std::io::Read;
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
use sdr::{Radio, Controller};
use ui::MainApp;

fn main() {
    let (mut control, reader) = rtlsdr::open(0).expect("unable to open rtlsdr");

    assert!(control.set_sample_rate(SDR_SAMPLE_RATE));
    assert!(control.set_ppm(-2));
    assert!(control.enable_agc());
    assert!(control.reset_buf());

    let mut conf = dirs::get_config_home().unwrap();
    conf.push("pi25");

    let talkgroups = {
        let mut conf = conf.clone();
        conf.push("talkgroups.csv");

        parse_talkgroups(File::open(conf).unwrap())
    };

    let sites = {
        let mut conf = conf.clone();
        conf.push("p25.toml");

        let mut toml = String::new();
        File::open(conf).unwrap().read_to_string(&mut toml).unwrap();

        Arc::new(parse_sites(&toml).unwrap())
    };

    if sites.len() == 0 {
        return;
    }

    let (tx_ui_ev, rx_ui_ev) = channel();
    let (tx_ctl_ev, rx_ctl_ev) = channel();
    let (tx_recv_ev, rx_recv_ev) = channel();
    let (tx_sdr_samp, rx_sdr_samp) = channel();
    let (tx_aud_ev, rx_aud_ev) = channel();

    let mut app = MainApp::new(talkgroups, sites.clone(), rx_ui_ev,
        tx_ctl_ev.clone(), tx_recv_ev.clone());
    let mut controller = Controller::new(control, rx_ctl_ev);
    let mut radio = Radio::new(tx_sdr_samp);
    let mut demod = Demod::new(rx_sdr_samp, tx_ui_ev.clone(), tx_recv_ev.clone());
    let mut audio = Audio::new(rx_aud_ev);
    let mut receiver = P25Receiver::new(sites.clone(), rx_recv_ev, tx_ui_ev.clone(),
        tx_ctl_ev.clone(), tx_aud_ev.clone());

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
        set_affinity(2);
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

fn set_affinity(cpu: usize) {
    unsafe {
        let mut cpus = std::mem::zeroed();

        libc::CPU_ZERO(&mut cpus);
        libc::CPU_SET(cpu, &mut cpus);

        assert!(libc::sched_setaffinity(0, std::mem::size_of_val(&cpus), &cpus) == 0);
    }
}
