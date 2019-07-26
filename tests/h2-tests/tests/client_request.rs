#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await)]

use h2_support::prelude::*;
use futures::compat::*;
use futures::{join, pending, poll, try_join};

#[runtime::test]
async fn handshake() {
    let _ = env_logger::try_init();

    let mock = mock_io::Builder::new()
        .handshake()
        .write(SETTINGS_ACK)
        .build();
    let mock = mock.compat();

    let (_client, h2) = client::handshake(mock)
        .compat()
        .await
        .unwrap();
    let h2 = h2.compat();

    log::trace!("hands have been shook");

    // At this point, the connection should be closed
    h2.await.unwrap();
}

#[runtime::test]
async fn client_other_thread() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .send_frame(frames::headers(1).response(200).eos())
        .close();

    let h2 = async {
        let (mut client, h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();
        let client = runtime::spawn(async move {
            let request = Request::builder()
                .uri("https://http2.akamai.com/")
                .body(())
                .unwrap();
            let res = client
                .send_request(request, true)
                .unwrap().0
                .compat()
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            Ok(())
        });

        try_join!(h2, client).unwrap();
    };

    join!(h2, srv);
}

#[runtime::test]
async fn recv_invalid_server_stream_id() {
    let _ = env_logger::try_init();

    let mock = mock_io::Builder::new()
        .handshake()
        // Write GET /
        .write(&[
            0, 0, 0x10, 1, 5, 0, 0, 0, 1, 0x82, 0x87, 0x41, 0x8B, 0x9D, 0x29,
                0xAC, 0x4B, 0x8F, 0xA8, 0xE9, 0x19, 0x97, 0x21, 0xE9, 0x84,
        ])
        .write(SETTINGS_ACK)
        // Read response
        .read(&[0, 0, 1, 1, 5, 0, 0, 0, 2, 137])
        // Write GO_AWAY
        .write(&[0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        .build();
    let mock = mock.compat();

    let (mut client, h2) = client::handshake(mock)
        .compat()
        .await
        .unwrap();

    // Send the request
    let request = Request::builder()
        .uri("https://http2.akamai.com/")
        .body(())
        .unwrap();

    log::info!("sending request");
    let (response, _) = client.send_request(request, true).unwrap();

    // The connection errors
    let h2 = h2.compat();
    assert!(h2.await.is_err());

    // The stream errors
    let response = response.compat();
    assert!(response.await.is_err());
}

#[runtime::test]
async fn request_stream_id_overflows() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let h2 = async {
        let (mut client, mut h2) = client::Builder::new()
            .initial_stream_id(::std::u32::MAX >> 1)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut h2 = h2.compat().map(Result::unwrap);

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // first request is allowed
        let (response, _) = client.send_request(request, true).unwrap();
        let response = response.compat();
        h2.drive(response).await.unwrap();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // second cannot use the next stream id, it's over
        let poll_err = client.poll_ready().unwrap_err();
        assert_eq!(poll_err.to_string(), "user error: stream ID overflowed");

        let err = client.send_request(request, true).unwrap_err();
        assert_eq!(err.to_string(), "user error: stream ID overflowed");

        // Hold on to the `client` handle to avoid sending a GO_AWAY
        // frame.
        h2.await;
    };

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(::std::u32::MAX >> 1)
                .request("GET", "https://example.com/")
                .eos(),
        )
        .send_frame(
            frames::headers(::std::u32::MAX >> 1)
                .response(200)
                .eos()
        )
        .idle_ms(10)
        .close();

    join!(h2, srv);
}

#[runtime::test]
async fn client_builder_max_concurrent_streams() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let mut settings = frame::Settings::default();
    settings.set_max_concurrent_streams(Some(1));

    let srv = srv
        .assert_client_handshake()
        .recv_custom_settings(settings)
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos()
        )
        .send_frame(frames::headers(1).response(200).eos())
        .close();

    let mut builder = client::Builder::new();
    builder.max_concurrent_streams(1);

    let h2 = async {
        let (mut client, mut h2) = builder
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut h2 = h2.compat().map(Result::unwrap);

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        let (response, _) = client.send_request(request, true).unwrap();
        let response = response.compat().map(Result::unwrap);
        join!(h2, response);
    };

    join!(h2, srv);
}

#[runtime::test]
async fn request_over_max_concurrent_streams_errors() {
    use futures01::future::Future as _;
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake_with_settings(frames::settings()
                // super tiny server
                .max_concurrent_streams(1))
        .recv_settings()
        .recv_frame(frames::headers(1).request("POST", "https://example.com/"))
        .send_frame(frames::headers(1).response(200))
        .recv_frame(frames::data(1, "hello").eos())
        .send_frame(frames::data(1, "").eos())
        .recv_frame(frames::headers(3).request("POST", "https://example.com/"))
        .send_frame(frames::headers(3).response(200))
        .recv_frame(frames::data(3, "hello").eos())
        .send_frame(frames::data(3, "").eos())
        .close();

    let h2 = async {
        let (mut client, h2) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let h2 = h2.compat().map(Result::unwrap);
        let mut h2 = h2.boxed();

        // Ensure we receive the server's settings
        // FIXME: Remove the .boxed here after std::future port is complete
        assert!(!yield_and_poll_once(h2.as_mut()).boxed().await.is_ready());

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // first request is allowed
        let (resp1, mut stream1) = client.send_request(request, false).unwrap();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // second request is put into pending_open
        let (resp2, mut stream2) = client.send_request(request, false).unwrap();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // third stream is over max concurrent
        assert!(client.poll_ready().expect("poll_ready").is_not_ready());

        let err = client.send_request(request, true).unwrap_err();
        assert_eq!(err.to_string(), "user error: rejected");

        stream1.send_data("hello".into(), true).expect("req send_data");

        let resp1 = resp1.compat();
        h2.drive(resp1).await.unwrap();
        stream2.send_data("hello".into(), true).unwrap();
        let resp2 = resp2.compat().map(Result::unwrap);
        join!(h2, resp2);
    };

    // FIXME: Remove this compat nonsense once std::future port is complete
    // This test ends up calling `task::current` deep in h2, which requires
    // a futures 0.1 executor to work.
    async {
        join!(h2, srv);
    }.map(Ok::<(), ()>).boxed().compat().wait().unwrap();
}

#[runtime::test]
async fn send_request_poll_ready_when_connection_error() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake_with_settings(frames::settings()
                // super tiny server
                .max_concurrent_streams(1))
        .recv_settings()
        .recv_frame(frames::headers(1).request("POST", "https://example.com/").eos())
        .send_frame(frames::headers(8).response(200).eos())
        //.recv_frame(frames::headers(5).request("POST", "https://example.com/").eos())
        .close();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut h2 = h2.compat().boxed();

        // Ensure we receive the server's settings
        // FIXME: Remove the .boxed here after std::future port is complete
        assert!(!yield_and_poll_once(h2.as_mut()).boxed().await.is_ready());

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // first request is allowed
        let (resp1, _) = client.send_request(request, true).unwrap();
        let resp1 = resp1.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        // second request is put into pending_open
        let (resp2, _) = client.send_request(request, true).unwrap();
        let resp2 = resp2.compat();

        // third stream is over max concurrent
        let until_ready = client.ready();
        let until_ready = until_ready.compat();

        // a FuturesUnordered is used on purpose!
        //
        // We don't want a join, since any of the other futures notifying
        // will make the until_ready future polled again, but we are
        // specifically testing that until_ready gets notified on its own.
        let mut unordered = futures::stream::FuturesUnordered::new();
        unordered.push(until_ready.map(Result::unwrap_err).boxed());
        unordered.push(h2.map(Result::unwrap_err).boxed());
        unordered.push(resp1.map(Result::unwrap_err).boxed());
        unordered.push(resp2.map(Result::unwrap_err).boxed());

        unordered.collect::<Vec<_>>().await;
    };

    join!(h2, srv);
}

#[runtime::test]
async fn send_reset_notifies_recv_stream() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://example.com/")
        )
        .send_frame(frames::headers(1).response(200))
        .recv_frame(frames::reset(1).refused())
        .recv_frame(frames::go_away(0))
        .recv_eof();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut conn = conn.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        let (resp1, mut tx) = client.send_request(request, false).unwrap();
        let resp1 = resp1.compat();
        let res = conn.drive(resp1).await.unwrap();

        let rx = res.into_body()
            .compat()
            .try_collect::<Vec<_>>()
            .map(Result::unwrap_err);
        let mut rx = conn.drive(rx).boxed();

        // Poll `rx` and yield back after calling `send_reset` to test
        // that `send_reset` is waking `rx` or `conn`.
        assert!(!poll!(rx.as_mut()).is_ready());
        tx.send_reset(h2::Reason::REFUSED_STREAM);
        pending!();

        rx.await;

        drop((client, tx)); // now let client gracefully goaway
        conn.await.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn http_11_request_without_scheme_or_authority() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "/")
                .scheme("http")
                .eos(),
        )
        .send_frame(frames::headers(1).response(200).eos())
        .close();

    let h2 = async {
        let (mut client, h2) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let h2 = h2.compat();

        // HTTP_11 request with just :path is allowed
        let request = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(())
            .unwrap();

        let (response, _) = client.send_request(request, true).unwrap();
        let response = response.compat();
        try_join!(h2, response).unwrap();
    };

    join!(h2, srv);
}

#[runtime::test]
async fn http_2_request_without_scheme_or_authority() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .close();

    let h2 = async {
        let (mut client, h2) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let h2 = h2.compat();

        // HTTP_2 with only a :path is illegal, so this request should
        // be rejected as a user error.
        let request = Request::builder()
            .version(Version::HTTP_2)
            .method(Method::GET)
            .uri("/")
            .body(())
            .unwrap();

        client
            .send_request(request, true)
            .expect_err("should be UserError");

        h2.await.unwrap();
    };

    join!(h2, srv);
}

#[test]
#[ignore]
fn request_with_h1_version() {}

#[runtime::test]
async fn request_with_connection_headers() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    // can't assert full handshake, since client never sends a request, and
    // thus never bothers to ack the settings...
    let srv = srv.read_preface()
        .recv_frame(frames::settings())
        // goaway is required to make sure the connection closes because
        // of no active streams
        .recv_frame(frames::go_away(0))
        .close();

    let headers = vec![
        ("connection", "foo"),
        ("keep-alive", "5"),
        ("proxy-connection", "bar"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "HTTP/2.0"),
        ("te", "boom"),
    ];

    let client = async {
        let (mut client, conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let conn = conn.compat();

        for (name, val) in headers {
            let req = Request::builder()
                .uri("https://http2.akamai.com/")
                .header(name, val)
                .body(())
                .unwrap();
            let err = client.send_request(req, true).expect_err(name);

            assert_eq!(err.to_string(), "user error: malformed headers");
        }

        drop(client);
        conn.await.unwrap()
    };

    join!(client, srv);
}

#[runtime::test]
async fn connection_close_notifies_response_future() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        // don't send any response, just close
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut conn = conn.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, _) = client
            .send_request(request, true)
            .unwrap();
        let response = response.compat();
        let response = conn.drive(response).await;
        assert_eq!(response.unwrap_err().to_string(), "broken pipe");
        conn.await.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn connection_close_notifies_client_poll_ready() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut conn = conn.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (req, _) = client
            .send_request(request, true)
            .unwrap();
        let req = req.compat();
        let res = conn.drive(req).await;

        assert_eq!(res.unwrap_err().to_string(), "broken pipe");
        assert_eq!(
            client.poll_ready().unwrap_err().to_string(),
            "broken pipe",
        );
        conn.await.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn sending_request_on_closed_connection() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .send_frame(frames::headers(1).response(200).eos())
        // a bad frame!
        .send_frame(frames::headers(0).response(200).eos())
        .close();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut h2 = h2.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // first request works
        let (response, _) = client
            .send_request(request, true)
            .unwrap();
        let response = response.compat();
        h2.drive(response).await.unwrap();
        // after finish request1, there should be a conn error
        h2.await.unwrap_err();

        let poll_err = client.poll_ready().unwrap_err();
        let msg = "protocol error: unspecific protocol error detected";
        assert_eq!(poll_err.to_string(), msg);

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let send_err = client.send_request(request, true).unwrap_err();
        assert_eq!(send_err.to_string(), msg);
    };

    join!(h2, srv);
}

#[runtime::test]
async fn recv_too_big_headers() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_custom_settings(
            frames::settings()
                .max_header_list_size(10)
        )
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .recv_frame(
            frames::headers(3)
                .request("GET", "https://http2.akamai.com/")
                .eos(),
        )
        .send_frame(frames::headers(1).response(200).eos())
        .send_frame(frames::headers(3).response(200))
        // no reset for 1, since it's closed anyways
        // but reset for 3, since server hasn't closed stream
        .recv_frame(frames::reset(3).refused())
        .idle_ms(10)
        .close();

    let client = async {
        let (mut client, conn) = client::Builder::new()
            .max_header_list_size(10)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let conn = conn.compat();
        let conn = conn.map(Result::unwrap);

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req1 = client
            .send_request(request, true)
            .unwrap()
            .0
            .compat()
            .map(|res| {
                assert_eq!(
                    res.unwrap_err().reason(),
                    Some(Reason::REFUSED_STREAM)
                );
            });

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req2 = client
            .send_request(request, true)
            .unwrap()
            .0
            .compat()
            .map(|err| {
                assert_eq!(
                    err.unwrap_err().reason(),
                    Some(Reason::REFUSED_STREAM)
                );
            });

        join!(conn, req1, req2);
    };

    join!(client, srv);
}

#[runtime::test]
async fn pending_send_request_gets_reset_by_peer_properly() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let payload = [0; (frame::DEFAULT_INITIAL_WINDOW_SIZE * 2) as usize];
    let max_frame_size = frame::DEFAULT_MAX_FRAME_SIZE as usize;

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/"),
        )
        // Note that we can only send up to ~4 frames of data by default
        .recv_frame(frames::data(1, &payload[0..max_frame_size]))
        .recv_frame(frames::data(1, &payload[max_frame_size..(max_frame_size*2)]))
        .recv_frame(frames::data(1, &payload[(max_frame_size*2)..(max_frame_size*3)]))
        .recv_frame(frames::data(1, &payload[(max_frame_size*3)..(max_frame_size*4-1)]))

        .idle_ms(100)

        .send_frame(frames::reset(1).refused())
        // Because all active requests are finished, connection should shutdown
        // and send a GO_AWAY frame. If the reset stream is bugged (and doesn't
        // count towards concurrency limit), then connection will not send
        // a GO_AWAY and this test will fail.
        .recv_frame(frames::go_away(0))

        .close();

    let client = async {
        let (mut client, mut conn) = client::Builder::new()
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut conn = conn.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client
            .send_request(request, false)
            .unwrap();
        let response = response.compat();

        // Send the data
        stream.send_data(payload[..].into(), true).unwrap();

        let err = conn.drive(response)
            .await
            .unwrap_err();
        assert_eq!(
            err.reason(),
            Some(Reason::REFUSED_STREAM)
        );

        drop((stream, client));
        conn.await.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn request_without_path() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frames::headers(1).request("GET", "http://example.com/").eos())
        .send_frame(frames::headers(1).response(200).eos())
        .close();

    let client = async {
        let (mut client, conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let conn = conn.compat();

        // Note the lack of trailing slash.
        let request = Request::get("http://example.com")
            .body(())
            .unwrap();

        let (response, _) = client.send_request(request, true).unwrap();
        let response = response.compat();
        try_join!(conn, response).unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn request_options_with_star() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    // Note the lack of trailing slash.
    let uri = uri::Uri::from_parts({
        let mut parts = uri::Parts::default();
        parts.scheme = Some(uri::Scheme::HTTP);
        parts.authority = Some(uri::Authority::from_shared("example.com".into()).unwrap());
        parts.path_and_query = Some(uri::PathAndQuery::from_static("*"));
        parts
    }).unwrap();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frames::headers(1).request("OPTIONS", uri.clone()).eos())
        .send_frame(frames::headers(1).response(200).eos())
        .close();

    let client = async {
        let (mut client, conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let conn = conn.compat();

        let request = Request::builder()
            .method(Method::OPTIONS)
            .uri(uri)
            .body(())
            .unwrap();

        let (response, _) = client.send_request(request, true).unwrap();
        let response = response.compat();
        try_join!(conn, response).unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn notify_on_send_capacity() {
    // This test ensures that the client gets notified when there is additional
    // send capacity. In other words, when the server is ready to accept a new
    // stream, the client is notified.
    use std::sync::{Arc, Barrier};

    let _ = env_logger::try_init();

    let (io, srv) = mock::new();
    let io = io.compat();
    let (done_tx, done_rx) = futures::channel::oneshot::channel();
    let barrier = Arc::new(Barrier::new(2));
    let srv_barrier = barrier.clone();

    let mut settings = frame::Settings::default();
    settings.set_max_concurrent_streams(Some(1));

    let srv = async move {
        let srv = srv.assert_client_handshake_with_settings(settings)
            // This is the ACK
            .recv_settings()
            .inspect(move |_| { srv_barrier.wait(); })
            .recv_frame(
                frames::headers(1)
                    .request("GET", "https://www.example.com/")
                    .eos(),
            )
            .send_frame(frames::headers(1).response(200).eos())
            .recv_frame(
                frames::headers(3)
                    .request("GET", "https://www.example.com/")
                    .eos(),
            )
            .send_frame(frames::headers(3).response(200).eos())
            .recv_frame(
                frames::headers(5)
                    .request("GET", "https://www.example.com/")
                    .eos(),
            )
            .send_frame(frames::headers(5).response(200).eos())
            .await;

        // Don't close the connection until the client is done doing its
        // checks.
        done_rx.await.unwrap();
        srv.close().await;
    };

    let client = async move {
        let (mut client, conn) = client::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let conn = conn.compat();

        let client = runtime::spawn(async move {
            barrier.wait();

            let mut responses = vec![];

            for _ in 0i32..3 {
                // Wait for capacity. If the client is **not** notified,
                // this hangs.
                client = client.ready()
                    .compat()
                    .await
                    .unwrap();

                let request = Request::builder()
                    .uri("https://www.example.com/")
                    .body(())
                    .unwrap();

                let (response, _) = client.send_request(request, true)
                    .unwrap();
                let response = response.compat();

                responses.push(response);
            }

            for response in responses {
                let response = response.await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
            }

            client.ready()
                .compat()
                .await
                .unwrap();

            done_tx.send(()).unwrap();
        });

        let (_, conn_res) = join!(client, conn);
        conn_res.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn send_stream_poll_reset() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv
        .assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://example.com/")
        )
        .send_frame(frames::reset(1).refused())
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://example.com/")
            .body(())
            .unwrap();

        let (_response, mut tx) = client.send_request(request, false).unwrap();
        let reset = tx.reset();
        let reset = reset.compat();

        let (_, reason) = try_join!(conn, reset).unwrap();
        assert_eq!(reason, Reason::REFUSED_STREAM);
    };

    join!(client, srv);
}

#[runtime::test]
async fn drop_pending_open() {
    use futures::channel::oneshot;

    // This test checks that a stream queued for pending open behaves correctly when its
    // client drops.
    let _ = env_logger::try_init();

    let (io, srv) = mock::new();
    let io = io.compat();
    let (init_tx, init_rx) = oneshot::channel();
    let (trigger_go_away_tx, trigger_go_away_rx) = oneshot::channel();
    let (sent_go_away_tx, sent_go_away_rx) = oneshot::channel();
    let (drop_tx, drop_rx) = oneshot::channel();

    let mut settings = frame::Settings::default();
    settings.set_max_concurrent_streams(Some(2));

    let srv = async {
        let mut srv = srv
            .assert_client_handshake_with_settings(settings)
            // This is the ACK
            .recv_settings()
            .await;
        init_tx.send(()).unwrap();
        srv.recv_frame(
            frames::headers(1)
                .request("GET", "https://www.example.com/"),
        ).await;
        srv.recv_frame(
            frames::headers(3)
                .request("GET", "https://www.example.com/")
                .eos(),
        ).await;
        trigger_go_away_rx.await.unwrap();
        srv.send_frame(frames::go_away(3)).await;
        sent_go_away_tx.send(()).unwrap();
        drop_rx.await.unwrap();
        srv.send_frame(frames::headers(3).response(200).eos()).await;
        srv.recv_frame(frames::data(1, vec![]).eos()).await;
        srv.send_frame(frames::headers(1).response(200).eos()).await;
        srv.close().await;
    };

    fn request() -> Request<()> {
        Request::builder()
            .uri("https://www.example.com/")
            .body(())
            .unwrap()
    }

    let client = async {
        let (mut client, mut conn) = client::Builder::new()
            .max_concurrent_reset_streams(0)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();
        let client = async {
            init_rx.await.unwrap();

            // Fill up the concurrent stream limit.
            // FIXME: Replace `client.poll_ready` with `poll!(client.ready())`
            assert!(client.poll_ready().unwrap().is_ready());
            let (response1, mut stream1) = client.send_request(request(), false).unwrap();
            assert!(client.poll_ready().unwrap().is_ready());
            let (response2, _) = client.send_request(request(), true).unwrap();
            assert!(client.poll_ready().unwrap().is_ready());
            let (response3, _) = client.send_request(request(), true).unwrap();

            // Trigger a GOAWAY frame to invalidate our third request.
            trigger_go_away_tx.send(()).unwrap();
            sent_go_away_rx.await.unwrap();

            // Now drop all the references to that stream.
            drop(response3);
            drop(client);
            drop_tx.send(()).unwrap();

            // Complete the second request, freeing up a stream.
            let response2 = response2.compat();
            response2.await.unwrap();

            stream1.send_data(Default::default(), true).unwrap();
            let response1 = response1.compat();
            response1.await.unwrap();

        };
        let (conn_res, _) = join!(conn, client);
        conn_res.unwrap();
    };

    join!(client, srv);
}

#[runtime::test]
async fn malformed_response_headers_dont_unlink_stream() {
    // This test checks that receiving malformed headers frame on a stream with
    // no remaining references correctly resets the stream, without prematurely
    // unlinking it.
    let _ = env_logger::try_init();

    let (io, srv) = mock::new();
    let io = io.compat();
    let (drop_tx, drop_rx) = futures::channel::oneshot::channel();
    let (queued_tx, queued_rx) = futures::channel::oneshot::channel();

    let srv = async {
        let mut srv = srv
            .assert_client_handshake()
            .recv_settings()
            .recv_frame(frames::headers(1).request("GET", "http://example.com/"))
            .recv_frame(frames::headers(3).request("GET", "http://example.com/"))
            .recv_frame(frames::headers(5).request("GET", "http://example.com/"))
            .await;
        drop_tx.send(()).unwrap();
        queued_rx.await.unwrap();
        srv.write_all(&[
            // 2 byte frame
            0, 0, 2,
            // type: HEADERS
            1,
            // flags: END_STREAM | END_HEADERS
            5,
            // stream identifier: 3
            0, 0, 0, 3,
            // data - invalid (pseudo not at end of block)
            144, 135
            // Per the spec, this frame should cause a stream error of type
            // PROTOCOL_ERROR.
        ]).await.unwrap();
        srv.close().await;
    };

    fn request() -> Request<()> {
        Request::builder()
            .uri("http://example.com/")
            .body(())
            .unwrap()
    }

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let client = async {
            let (_, mut send1) = client.send_request(
                request(), false).unwrap();
            // Use up most of the connection window.
            send1.send_data(vec![0; 65534].into(), true).unwrap();
            let (_, mut send2) = client.send_request(
                request(), false).unwrap();
            let (_, mut send3) = client.send_request(
                request(), false).unwrap();

            drop_rx.await.unwrap();
            drop(send1);

            // Use up the remainder of the connection window.
            send2.send_data(vec![0; 2].into(), true).unwrap();
            // Queue up for more connection window.
            send3.send_data(vec![0; 1].into(), true).unwrap();
            queued_tx.send(()).unwrap();
            Ok(())
        };
        try_join!(conn, client).unwrap();
    };

    join!(client, srv);
}

const SETTINGS_ACK: &'static [u8] = &[0, 0, 0, 4, 1, 0, 0, 0, 0];

/// Polls a future once after yielding
///
/// Before yielding, `wake` will be called so this future will
/// always be polled again
async fn yield_and_poll_once<F: Future + Unpin>(f: F) -> Poll<F::Output> {
    Wake.await;
    pending!();
    poll!(f)
}

use std::future::Future;
use std::task::*;
use std::pin::Pin;
struct Wake;

impl Future for Wake {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        cx.waker().wake_by_ref();
        Poll::Ready(())
    }
}
