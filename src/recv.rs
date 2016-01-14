extern crate byteorder;
extern crate byteorder_iter;
extern crate dsp;
extern crate imbe;
extern crate map_in_place;
extern crate p25;

use std::fs::{OpenOptions, File};
use std::io::Write;

use byteorder::{ReadBytesExt};
use byteorder_iter::{IterBytesExt};
use imbe::decoder::{IMBEDecoder, CAIFrame};
use map_in_place::MapInPlace;
use p25::filters::ReceiveFilter;
use p25::receiver;
use p25::receiver::ReceiverEvent;
use p25::status::StreamSymbol;
use p25::trunking::tsbk::{self, TSBKOpcode};
use p25::voice::control::{self, LinkControlOpcode};
use p25::voice::FrameGroupEvent;
use p25::voice::{self, frame};

use p25::nid::DataUnit::*;
use self::State::*;

enum State {
    Idle,
    DecodeHeader(voice::VoiceHeaderReceiver),
    DecodeLCFrameGroup(voice::VoiceLCFrameGroupReceiver),
    DecodeCCFrameGroup(voice::VoiceCCFrameGroupReceiver),
    DecodeLCTerminator(voice::VoiceLCTerminatorReceiver),
    DecodeTSBK(tsbk::TSBKReceiver),
}

fn handle_lc(lc: control::LinkControlFields) {
    println!("crypto:{:?}", lc.protected());
    println!("fmt:{:?}", lc.opcode());

    match lc.opcode().unwrap() {
        LinkControlOpcode::CallTermination => {
            let dec = control::CallTermination::new(lc);
            println!("  unit:{:024b}", dec.unit());
        },
        LinkControlOpcode::AdjacentSiteBroadcast => {
            let dec = control::AdjacentSiteBroadcast::new(lc);
            let ch = dec.channel();
            println!("  area:{}", dec.area());
            println!("  system:{:x}", dec.system());
            println!("  rfss:{:x}", dec.rfss());
            println!("  site:{:x}", dec.site());
            println!("  channel");
            println!("    band:{}", ch.band());
            println!("    number:{}", ch.number());
            println!("  services:{:?}", dec.services());
        },
        LinkControlOpcode::GroupVoiceTraffic => {
            let dec = control::GroupVoiceTraffic::new(lc);
            let opts = dec.opts();
            println!("  mfg:{}", dec.mfg());
            println!("  emerg:{}", opts.emergency());
            println!("  duplex:{}", opts.duplex());
            println!("  pkt:{}", opts.packet_switched());
            println!("  prio:{:03b}", opts.prio());
            println!("  talkgroup:{:?}", dec.talk_group());
            println!("  unit:{:024b}", dec.src_unit());
        },
        _ => {}
    }
}

fn handle_tsbk(tsbk: tsbk::TSBKFields) {
    println!("TRUNKING SIGNALING");
    println!("  prot:{} opcode:{:?}", tsbk.protected(), tsbk.opcode());
    println!("  mfg:{:08b}", tsbk.mfg());
    println!("  crc:{:016b}", tsbk.crc());
    println!("      {:016b}", tsbk.calc_crc());

    if tsbk.mfg() != 0 {
        println!("  NONSTANDARD MFG");
        return;
    }

    match tsbk.opcode().unwrap() {
        TSBKOpcode::UnitCallRequest => {
            let dec = tsbk::UnitCallRequest::new(tsbk);
            println!("    src:{:x}", dec.src_id());
            println!("    dest:{:x}", dec.dest_unit());
        },
        TSBKOpcode::AltControlBroadcast => {
            let dec = tsbk::AltControlBroadcast::new(tsbk);
            let ch1 = dec.channel_a();
            let ch2 = dec.channel_b();
            println!("    rfss:{:x}", dec.rfss());
            println!("    site:{:x}", dec.site());
            println!("    channel A");
            println!("      band:{}", ch1.band());
            println!("      number:{}", ch1.number());
            println!("      services:{:?}", dec.services_a());
            println!("    channel B");
            println!("      band:{}", ch2.band());
            println!("      number:{}", ch2.number());
            println!("      services:{:?}", dec.serviced_b());
        },
        TSBKOpcode::AdjacentSiteBroadcast | TSBKOpcode::RFSSStatusBroadcast => {
            let dec = tsbk::SiteStatusBroadcast::new(tsbk);
            let ch = dec.channel();
            let svc = dec.services();
            println!("    area:{:x}", dec.area());
            println!("    system:{:x}", dec.system());
            println!("    rfss:{:x}", dec.rfss());
            println!("    site:{:x}", dec.site());
            println!("    channel");
            println!("      band:{}", ch.band());
            println!("      number:{}", ch.number());
            println!("    services");
            println!("      composite:{}", svc.is_composite());
            println!("      updates:{}", svc.has_updates());
            println!("      backup:{}", svc.is_backup());
            println!("      data:{}", svc.has_data());
            println!("      voice:{}", svc.has_voice());
            println!("      reg:{}", svc.has_registration());
            println!("      auth:{}", svc.has_auth());
        },
        TSBKOpcode::ChannelParamsUpdate => {
            let dec = tsbk::ChannelParamsUpdate::new(tsbk);
            let p = dec.params();
            println!("    tx:{}Hz", p.tx_freq);
            println!("    rx:{}Hz", p.rx_freq);
            println!("    bw:{}Hz", p.bandwidth);
        },
        TSBKOpcode::NetworkStatusBroadcast => {
            let dec = tsbk::NetworkStatusBroadcast::new(tsbk);
            let ch = dec.channel();
            let svc = dec.services();
            println!("    area:{:x}", dec.area());
            println!("    wacn:{:x}", dec.wacn());
            println!("    system:{:x}", dec.system());
            println!("    channel");
            println!("      band:{}", ch.band());
            println!("      number:{}", ch.number());
            println!("    services");
            println!("      composite:{}", svc.is_composite());
            println!("      updates:{}", svc.has_updates());
            println!("      backup:{}", svc.is_backup());
            println!("      data:{}", svc.has_data());
            println!("      voice:{}", svc.has_voice());
            println!("      reg:{}", svc.has_registration());
            println!("      auth:{}", svc.has_auth());
        },
        TSBKOpcode::GroupVoiceUpdate => {
            let dec = tsbk::GroupVoiceUpdate::new(tsbk);
            let ch1 = dec.channel_a();
            let ch2 = dec.channel_b();
            println!("    talkgroup 1:{:?}", dec.talk_group_a());
            println!("    channel 1");
            println!("      band:{}", ch1.band());
            println!("      number:{}", ch1.number());
            println!("    talkgroup 2:{:?}", dec.talk_group_b());
            println!("    channel 2");
            println!("      band:{}", ch2.band());
            println!("      number:{}", ch2.number());
        },
        _ => {
            println!("    NOT HANDLED");
        },
    }
}

fn main() {
    let mut recv = receiver::DataUnitReceiver::new();
    let mut filt = ReceiveFilter::new();
    let mut imbe = IMBEDecoder::new();
    let mut output = std::io::BufWriter::new(File::create("imbe2.fifo").unwrap());
    let mut output = std::io::BufWriter::new(OpenOptions::new().write(true).open("imbe2.fifo").unwrap());

    let mut handle_frame = |vf: frame::VoiceFrame| {
        let frame = CAIFrame::new(vf.chunks, vf.errors);
        let mut samples = [0.0; imbe::consts::SAMPLES];

        imbe.decode(frame, &mut samples);
        samples.map_in_place(|&s| s / 8192.0);

        println!("write");
        output.write_all(unsafe {
            std::slice::from_raw_parts(samples.as_ptr() as *const u8,
                samples.len() * 4)
        }).unwrap();
    };

    let mut state = Idle;

    let stdin = std::io::stdin();
    let samples = stdin.iter_f32().map(|x| filt.feed(x));

    for (t, s) in samples.enumerate() {
        let event = match recv.feed(s) {
            Some(Ok(event)) => event,
            Some(Err(err)) => {
                println!("ERROR: {:?}", err);
                recv.resync();
                continue;
            },
            None => continue,
        };

        let dibit = match event {
            ReceiverEvent::NetworkID(nid) => {
                state = match nid.data_unit() {
                    VoiceHeader =>
                        DecodeHeader(voice::VoiceHeaderReceiver::new()),
                    VoiceSimpleTerminator => {
                        recv.flush_pads();
                        Idle
                    },
                    VoiceLCTerminator =>
                        DecodeLCTerminator(voice::VoiceLCTerminatorReceiver::new()),
                    VoiceLCFrameGroup =>
                        DecodeLCFrameGroup(voice::VoiceLCFrameGroupReceiver::new()),
                    VoiceCCFrameGroup =>
                        DecodeCCFrameGroup(voice::VoiceCCFrameGroupReceiver::new()),
                    TrunkingSignaling =>
                        DecodeTSBK(tsbk::TSBKReceiver::new()),
                    DataPacket => {
                        recv.resync();
                        Idle
                    },
                };

                continue;
            },
            ReceiverEvent::Symbol(StreamSymbol::Status(s)) => {
                continue;
            },
            ReceiverEvent::Symbol(StreamSymbol::Data(dibit)) => dibit,
        };

        match state {
            DecodeHeader(ref mut head) => match head.feed(dibit) {
                Some(Ok(h)) => {
                    recv.flush_pads();
                },
                Some(Err(err)) => {
                    recv.resync();
                },
                None => {},
            },
            DecodeLCFrameGroup(ref mut fg) => match fg.feed(dibit) {
                Some(Ok(event)) => match event {
                    FrameGroupEvent::VoiceFrame(vf) => {
                        handle_frame(vf);

                        if fg.done() {
                            recv.flush_pads();
                        }
                    },
                    FrameGroupEvent::Extra(lc) => {
                    },
                    FrameGroupEvent::DataFragment(data) => {
                    },
                },
                Some(Err(err)) => {
                    recv.resync();
                },
                None => {},
            },
            DecodeCCFrameGroup(ref mut fg) => match fg.feed(dibit) {
                Some(Ok(event)) => match event {
                    FrameGroupEvent::VoiceFrame(vf) => {
                        handle_frame(vf);

                        if fg.done() {
                            recv.flush_pads();
                        }
                    },
                    FrameGroupEvent::Extra(cc) => {
                    },
                    FrameGroupEvent::DataFragment(data) => {
                    },
                },
                Some(Err(err)) => {
                    recv.resync();
                },
                None => {},
            },
            DecodeLCTerminator(ref mut term) => match term.feed(dibit) {
                Some(Ok(lc)) => {
                    recv.flush_pads();
                },
                Some(Err(err)) => {
                    recv.resync();
                },
                None => {},
            },
            DecodeTSBK(ref mut dec) => match dec.feed(dibit) {
                Some(Ok(tsbk)) => {
                    handle_tsbk(tsbk);

                    if tsbk.is_tail() {
                        recv.flush_pads();
                    }
                },
                Some(Err(err)) => {
                    recv.resync();
                },
                None => {},
            },
            Idle => {},
        }
    }
}
