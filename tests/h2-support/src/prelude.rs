
// Re-export H2 crate
pub use h2;

pub use h2::*;
pub use h2::client;
pub use h2::frame::StreamId;
pub use h2::server;

// Re-export mock
pub use super::mock::{self, HandleFutureExt};

// Re-export frames helpers
pub use super::frames;

// Re-export mock notify
pub use super::notify::MockNotify;

// Re-export utility mod
pub use super::util;

// Re-export some type defines
pub use super::{Codec, SendFrame};

// Re-export macros
pub use super::{assert_ping, assert_data, assert_headers, assert_closed,
                raw_codec, poll_frame, poll_err};

// Re-export useful crates
pub use {bytes, env_logger, futures, http, tokio_io};
pub use super::mock_io;

// Re-export primary future types
pub use futures::prelude::*;

// And our Future extensions
pub use super::future_ext::{FutureExt as _, Unwrap};

// Our client_ext helpers
pub use super::client_ext::{SendRequestExt};

// Re-export HTTP types
pub use http::{uri, HeaderMap, Method, Request, Response, StatusCode, Version};

pub use bytes::{Buf, BufMut, Bytes, BytesMut, IntoBuf};

pub use tokio_io::{AsyncRead, AsyncWrite};

pub use std::thread;
pub use std::time::Duration;

// ===== Everything under here shouldn't be used =====
// TODO: work on deleting this code

pub use futures::future::poll_fn;

pub trait MockH2 {
    fn handshake(&mut self) -> &mut Self;
}

impl MockH2 for super::mock_io::Builder {
    fn handshake(&mut self) -> &mut Self {
        self.write(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            // Settings frame
            .write(frames::SETTINGS)
            .read(frames::SETTINGS)
            .read(frames::SETTINGS_ACK)
    }
}

pub fn build_large_headers() -> Vec<(&'static str, String)> {
    vec![
        ("one", "hello".to_string()),
        ("two", build_large_string('2', 4 * 1024)),
        ("three", "three".to_string()),
        ("four", build_large_string('4', 4 * 1024)),
        ("five", "five".to_string()),
        ("six", build_large_string('6', 4 * 1024)),
        ("seven", "seven".to_string()),
        ("eight", build_large_string('8', 4 * 1024)),
        ("nine", "nine".to_string()),
        ("ten", build_large_string('0', 4 * 1024)),
    ]
}

fn build_large_string(ch: char, len: usize) -> String {
    let mut ret = String::new();

    for _ in 0..len {
        ret.push(ch);
    }

    ret
}
