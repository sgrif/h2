#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await)]

use h2_support::prelude::*;
use futures::compat::*;
use futures::join;

use std::error::Error;

#[runtime::test]
async fn read_none() {
    let io = mock_io::Builder::new().build();
    let io = io.compat();
    let mut codec = Codec::from(io);
    let mut codec = codec.compat();

    assert_closed!(codec);
}

#[runtime::test]
#[ignore]
async fn read_frame_too_big() {}

// ===== DATA =====

#[runtime::test]
async fn read_data_no_padding() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 5, 0, 0, 0, 0, 0, 1,
            "hello",
        ];
    };

    let data = poll_frame!(Data, codec);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"hello"[..]);
    assert!(!data.is_end_stream());

    assert_closed!(codec);
}

#[runtime::test]
async fn read_data_empty_payload() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 0, 0, 0, 0, 0, 0, 1,
        ];
    };

    let data = poll_frame!(Data, codec);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b""[..]);
    assert!(!data.is_end_stream());

    assert_closed!(codec);
}

#[runtime::test]
async fn read_data_end_stream() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 5, 0, 1, 0, 0, 0, 1,
            "hello",
        ];
    };

    let data = poll_frame!(Data, codec);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"hello"[..]);
    assert!(data.is_end_stream());

    assert_closed!(codec);
}

#[runtime::test]
async fn read_data_padding() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 16, 0, 0x8, 0, 0, 0, 1,
            5,       // Pad length
            "helloworld", // Data
            "\0\0\0\0\0", // Padding
        ];
    };

    let data = poll_frame!(Data, codec);
    assert_eq!(data.stream_id(), 1);
    assert_eq!(data.payload(), &b"helloworld"[..]);
    assert!(!data.is_end_stream());

    assert_closed!(codec);
}

#[runtime::test]
async fn read_push_promise() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 0x5,
            0x5, 0x4,
            0, 0, 0, 0x1, // stream id
            0, 0, 0, 0x2, // promised id
            0x82, // HPACK :method="GET"
        ];
    };

    let pp = poll_frame!(PushPromise, codec);
    assert_eq!(pp.stream_id(), 1);
    assert_eq!(pp.promised_id(), 2);
    assert_eq!(pp.into_parts().0.method, Some(Method::GET));

    assert_closed!(codec);
}

#[runtime::test]
async fn read_data_stream_id_zero() {
    let mut codec = raw_codec! {
        read => [
            0, 0, 5, 0, 0, 0, 0, 0, 0,
            "hello", // Data
        ];
    };

    poll_err!(codec);
}

// ===== HEADERS =====

#[runtime::test]
#[ignore]
async fn read_headers_without_pseudo() {}

#[runtime::test]
#[ignore]
async fn read_headers_with_pseudo() {}

#[runtime::test]
#[ignore]
async fn read_headers_empty_payload() {}

#[runtime::test]
async fn read_continuation_frames() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let large = build_large_headers();
    let frame = large.iter().fold(
        frames::headers(1).response(200),
        |frame, &(name, ref value)| frame.field(name, &value[..]),
    ).eos();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .send_frame(frame)
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, _) = client
            .send_request(request, true)
            .unwrap();
        let response = response.compat();

        let res = conn.drive(response).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let (head, _body) = res.into_parts();
        let expected = large.iter().fold(HeaderMap::new(), |mut map, &(name, ref value)| {
            use h2_support::frames::HttpTryInto;
            map.append(name, value.as_str().try_into().unwrap());
            map
        });
        assert_eq!(head.headers, expected);

        conn.await.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn update_max_frame_len_at_rest() {
    let _ = env_logger::try_init();
    // TODO: add test for updating max frame length in flight as well?
    let mut codec = raw_codec! {
        read => [
            0, 0, 5, 0, 0, 0, 0, 0, 1,
            "hello",
            0, 64, 1, 0, 0, 0, 0, 0, 1,
            vec![0; 16_385],
        ];
    };

    assert_eq!(poll_frame!(Data, codec).payload(), &b"hello"[..]);

    codec.get_mut().set_max_recv_frame_size(16_384);

    assert_eq!(codec.get_ref().max_recv_frame_size(), 16_384);
    let err = codec.next()
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err.description(),
        "frame with invalid size"
    );
}
