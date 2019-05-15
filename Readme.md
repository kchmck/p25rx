# p25rx – P25 receiver

[![CircleCI](https://img.shields.io/circleci/project/github/kchmck/p25rx/master.svg)](https://circleci.com/gh/kchmck/p25rx)

This project implements a real-time Project 25 receiver in Rust. The receiver is
trunking-aware and hops between control and traffic channels as talkgroup conversations
begin and end.

## Building

For a Linux machine, use the following build steps:

1. Install [nightly Rust](https://www.rust-lang.org/). If [rustup](https://rustup.rs/) is
   used, nightly can be installed with

   ```sh
   rustup default nightly
   ```

2. Install the `librtlsdr` system library (`pacman -S rtl-sdr` on Archlinux or `apt-get
   install librtlsdr-dev` on Ubuntu), and verify that the library is recognized by
   `pkg-config`:

   ```sh
   pkg-config --modversion librtlsdr
   ```

3. Build the project by running

   ```sh
   cargo rustc --release -- -C target-cpu=native -C lto
   ```

   within the project root. This will compile an optimized binary for the current machine
   and place it at `target/release/p25rx`.

## Usage

The program is typically ran with a command like
```
./target/release/p25rx -f 856162500 -p=-2 -g auto -a p25.fifo
```
This receives P25 using the control channel at 856.1625MHz, setting the RTL-SDR frequency
correction to -2ppm, enabling the hardware automatic gain control, and writing any
decoded voice frames into the `p25.fifo` pipe. These options are explained more in the
following sections.

### Audio output

Audio samples are written out in the following raw PCM format:

 - 8kHz sample rate
 - 32-bit float samples in native endianness
 - Mono channel

The stream of samples is written into the file specified by `-a`, which will typically be
a [FIFO pipe](https://en.wikipedia.org/wiki/Named_pipe) connected to an audio output
command like Pulseaudio's
[`paplay`](http://manpages.ubuntu.com/manpages/zesty/man1/paplay.1.html) or Alsa's
[`aplay`](http://manpages.ubuntu.com/manpages/zesty/man1/aplay.1.html).

For example, create the pipe with
```
mkfifo p25.fifo
```
Then, stream the samples to Pulseaudio:
```
paplay -p --raw --rate 8000 --format float32le --channels 1 p25.fifo
```
or to Alsa:
```
aplay -t raw -r 8000 -f FLOAT_LE -c 1 p25.fifo
```
These commands will run until the receiver exits.

Note that `paplay` can be installed with the `libpulse` package on Archlinux and the
`pulseaudio-utils` package on Ubuntu, and `aplay` can be installed with the `alsa-utils`
package on both.

To disable audio output, pass in `-a /dev/null`.
