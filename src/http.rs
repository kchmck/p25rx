//! HTTP response utilities.

use std::io::{Write, BufWriter};
use std;

use chrono::UTC;
use serde::Serialize;
use serde_json;
use uhttp_chunked_write::ChunkedWrite;
use uhttp_response_header::HeaderLines;
use uhttp_status::StatusCode;
use uhttp_version::HttpVersion;

/// Send common response headers starting with the given status code.
pub fn send_status<W: Write>(s: W, st: StatusCode) -> std::io::Result<()> {
    send_head(&mut HeaderLines::new(s), st)
}

/// Write common response headers into the given sink.
pub fn send_head<W: Write>(h: &mut HeaderLines<W>, st: StatusCode) -> std::io::Result<()> {
    try!(write!(h.line(), "{} {}", HttpVersion::from_parts(1, 1), st));
    try!(write!(h.line(), "Date: {}", UTC::now().format("%a, %d %b %Y %T %Z")));
    try!(write!(h.line(), "Access-Control-Allow-Origin: *"));

    Ok(())
}

/// Send the given message as a JSON response body.
pub fn send_json<W: Write, S: Serialize>(mut s: W, msg: S) -> std::io::Result<()> {
    {
        let mut h = HeaderLines::new(&mut s);
        try!(send_head(&mut h, StatusCode::Ok));
        try!(write!(h.line(), "Content-Type: application/json"));
        try!(write!(h.line(), "Transfer-Encoding: chunked"));
    }

    let mut body = BufWriter::new(ChunkedWrite::new(s));

    serde_json::to_writer(&mut body, &msg)
        .map_err(|_| std::io::ErrorKind::Other.into())
}
