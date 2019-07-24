#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await)]

use h2_support::prelude::*;
use futures::compat::*;
use futures::{join, try_join};
use futures::prelude::*;

const SETTINGS: &'static [u8] = &[0, 0, 0, 4, 0, 0, 0, 0, 0];
const SETTINGS_ACK: &'static [u8] = &[0, 0, 0, 4, 1, 0, 0, 0, 0];

#[runtime::test]
async fn read_preface_in_multiple_frames() {
    let _ = env_logger::try_init();

    let mock = mock_io::Builder::new()
        .read(b"PRI * HTTP/2.0")
        .read(b"\r\n\r\nSM\r\n\r\n")
        .write(SETTINGS)
        .read(SETTINGS)
        .write(SETTINGS_ACK)
        .read(SETTINGS_ACK)
        .build();
    let mock = mock.compat();

    let h2 = server::handshake(mock)
        .compat()
        .await
        .unwrap();
    let mut h2 = h2.compat();

    assert!(h2.next().await.is_none());
}

#[runtime::test]
async fn server_builder_set_max_concurrent_streams() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let mut settings = frame::Settings::default();
    settings.set_max_concurrent_streams(Some(1));

    let client = client
        .assert_server_handshake()
        .recv_custom_settings(settings)
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/"),
        )
        .send_frame(
            frames::headers(3)
                .request("GET", "https://example.com/"),
        )
        .send_frame(frames::data(1, &b"hello"[..]).eos(),)
        .recv_frame(frames::reset(3).refused())
        .recv_frame(frames::headers(1).response(200).eos())
        .close();

    let mut builder = server::Builder::new();
    builder.max_concurrent_streams(1);

    let h2 = async {
        let mut srv = builder
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();
        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.method(), &http::Method::GET);

        let rsp =
            http::Response::builder()
                .status(200).body(())
                .unwrap();
        stream.send_response(rsp, true).unwrap();

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap();
    };

    join!(h2, client);
}

#[runtime::test]
async fn serve_request() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos(),
        )
        .recv_frame(frames::headers(1).response(200).eos())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();
        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.method(), &http::Method::GET);

        let rsp = http::Response::builder().status(200).body(()).unwrap();
        stream.send_response(rsp, true).unwrap();

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}

#[test]
#[ignore]
fn accept_with_pending_connections_after_socket_close() {}

#[runtime::test]
async fn recv_invalid_authority() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let bad_auth = util::byte_str("not:a/good authority");
    let mut bad_headers: frame::Headers = frames::headers(1)
        .request("GET", "https://example.com/")
        .eos()
        .into();
    bad_headers.pseudo_mut().authority = Some(bad_auth);

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(bad_headers)
        .recv_frame(frames::reset(1).protocol_error())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");

        srv.close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
async fn recv_connection_header() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let req = |id, name, val| {
        frames::headers(id)
            .request("GET", "https://example.com/")
            .field(name, val)
            .eos()
    };

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(req(1, "connection", "foo"))
        .send_frame(req(3, "keep-alive", "5"))
        .send_frame(req(5, "proxy-connection", "bar"))
        .send_frame(req(7, "transfer-encoding", "chunked"))
        .send_frame(req(9, "upgrade", "HTTP/2.0"))
        .recv_frame(frames::reset(1).protocol_error())
        .recv_frame(frames::reset(3).protocol_error())
        .recv_frame(frames::reset(5).protocol_error())
        .recv_frame(frames::reset(7).protocol_error())
        .recv_frame(frames::reset(9).protocol_error())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");

        srv.close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
// FIXME: This test name does not describe the condition it's testing
async fn sends_reset_cancel_when_req_body_is_dropped() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("POST", "https://example.com/")
        )
        .recv_frame(frames::headers(1).response(200).eos())
        .recv_frame(frames::reset(1).cancel())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();
        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.method(), &http::Method::POST);

        let rsp = http::Response::builder().status(200).body(()).unwrap();
        stream.send_response(rsp, true).unwrap();

        drop((req, stream));

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap()
    };

    join!(srv, client);
}

#[runtime::test]
async fn abrupt_shutdown() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("POST", "https://example.com/")
        )
        .recv_frame(frames::go_away(1).internal_error())
        .recv_eof();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();
        let (req, tx) = srv.next().await.expect("server receives request").unwrap();

        let req_fut = req
            .into_body()
            .compat()
            .try_concat()
            .inspect(|_| drop(tx))
            .map(|res| {
                let err = res.expect_err("request body should error");
                assert_eq!(
                    err.reason(),
                    Some(Reason::INTERNAL_ERROR),
                    "streams should be also error with user's reason",
                );
            });

        let srv_fut = srv
            .get_mut()
            .abrupt_shutdown(Reason::INTERNAL_ERROR)
            .compat()
            .map(Result::unwrap);

        join!(req_fut, srv_fut);
    };

    join!(srv, client);
}

#[runtime::test]
async fn graceful_shutdown() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos(),
        )
        // 2^31 - 1 = 2147483647
        // Note: not using a constant in the library in order to test that
        // the constant has the correct value
        .recv_frame(frames::go_away(2147483647))
        .recv_frame(frames::ping(frame::Ping::SHUTDOWN))
        .recv_frame(frames::headers(1).response(200).eos())
        // Pretend this stream was sent while the GOAWAY was in flight
        .send_frame(
            frames::headers(3)
                .request("POST", "https://example.com/"),
        )
        .send_frame(frames::ping(frame::Ping::SHUTDOWN).pong())
        .recv_frame(frames::go_away(3))
        // streams sent after GOAWAY receive no response
        .send_frame(
            frames::headers(7)
                .request("GET", "https://example.com/"),
        )
        .send_frame(frames::data(7, "").eos())
        .send_frame(frames::data(3, "").eos())
        .recv_frame(frames::headers(3).response(200).eos())
        .recv_eof();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();
        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.method(), &http::Method::GET);

        let _ = srv.get_mut().graceful_shutdown();

        let rsp = http::Response::builder()
            .status(200)
            .body(())
            .unwrap();
        stream.send_response(rsp, true).unwrap();

        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.method(), &http::Method::POST);
        let body = req.into_parts().1;

        let body = async {
            let buf = body
                .compat()
                .map(Result::unwrap)
                .concat()
                .await;
            assert!(buf.is_empty());

            let rsp = http::Response::builder()
                .status(200)
                .body(())
                .unwrap();
            stream.send_response(rsp, true)
        };

        let srv = srv.get_mut()
            .close()
            .compat();

        try_join!(srv, body).unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
async fn sends_reset_cancel_when_res_body_is_dropped() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos()
        )
        .recv_frame(frames::headers(1).response(200))
        .recv_frame(frames::reset(1).cancel())
        .send_frame(
            frames::headers(3)
                .request("GET", "https://example.com/")
                .eos()
        )
        .recv_frame(frames::headers(3).response(200))
        .recv_frame(frames::data(3, vec![0; 10]))
        .recv_frame(frames::reset(3).cancel())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();

        {
            let (req, mut stream) = srv.next().await.unwrap().unwrap();

            assert_eq!(req.method(), &http::Method::GET);

            let rsp = http::Response::builder()
                .status(200)
                .body(())
                .unwrap();
            stream.send_response(rsp, false).unwrap();
        }
        {
            let (_req, mut stream) = srv.next().await.unwrap().unwrap();

            let rsp = http::Response::builder()
                .status(200)
                .body(())
                .unwrap();
            let mut tx = stream.send_response(rsp, false).unwrap();
            tx.send_data(vec![0; 10].into(), false).unwrap();
            // no send_data with eos
        }

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
async fn too_big_headers_sends_431() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_custom_settings(
            frames::settings()
                .max_header_list_size(10)
        )
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .field("some-header", "some-value")
                .eos()
        )
        .recv_frame(frames::headers(1).response(431).eos())
        .idle_ms(10)
        .close();

    let srv = async {
        let mut srv = server::Builder::new()
            .max_header_list_size(10)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");

        srv.close()
            .compat()
            .await
            .unwrap();
    };
    join!(client, srv);
}

#[runtime::test]
async fn too_big_headers_sends_reset_after_431_if_not_eos() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_custom_settings(
            frames::settings()
                .max_header_list_size(10)
        )
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .field("some-header", "some-value")
        )
        .recv_frame(frames::headers(1).response(431).eos())
        .recv_frame(frames::reset(1).refused())
        .close();

    let srv = async {
        let mut srv = server::Builder::new()
            .max_header_list_size(10)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");

        srv.close()
            .compat()
            .await
            .unwrap();
    };
    join!(client, srv);
}

#[runtime::test]
async fn poll_reset() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos()
        )
        .idle_ms(10)
        .send_frame(frames::reset(1).cancel())
        .close();

    let srv = async {
        let mut srv = server::Builder::new()
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .unwrap();
        let mut srv = srv.compat();

        let (_req, mut tx) = srv.next().await.unwrap().unwrap();

        let conn_closed = srv.get_mut()
            .close()
            .compat()
            .map(Result::unwrap);
        let stream_reset = tx.client_reset()
            .compat()
            .map_ok(|reason| assert_eq!(reason, Reason::CANCEL))
            .map(Result::unwrap);
        join!(conn_closed, stream_reset);
    };

    join!(srv, client);
}

#[runtime::test]
async fn poll_reset_io_error() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos()
        )
        .idle_ms(10)
        .close();

    let srv = async {
        let mut srv = server::Builder::new()
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();

        let (_req, mut tx) = srv.next().await.unwrap().unwrap();

        let conn_closed = srv.get_mut()
            .close()
            .compat()
            .map(Result::unwrap);

        let stream_reset = tx.client_reset();
        let stream_reset = stream_reset.compat();

        let (_, reset) = join!(conn_closed, stream_reset);
        reset.expect_err("poll_reset should error");
    };

    join!(srv, client);
}

#[runtime::test]
async fn poll_reset_after_send_response_is_user_error() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "https://example.com/")
                .eos()
        )
        .recv_frame(
            frames::headers(1)
                .response(200)
        )
        .recv_frame(
            // After the error, our server will drop the handles,
            // meaning we receive a RST_STREAM here.
            frames::reset(1).cancel()
        )
        .idle_ms(10)
        .close();

    let srv = async {
        let mut srv = server::Builder::new()
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();

        {
            let (_req, mut tx) = srv.next().await.unwrap().unwrap();

            tx.send_response(Response::new(()), false).expect("response");
            tx.client_reset()
                .compat()
                .await
                .expect_err("poll_reset should error");
        }

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
async fn server_error_on_unclean_shutdown() {
    let _ = env_logger::try_init();
    let (io, mut client) = mock::new();
    let io = io.compat();

    let srv = server::Builder::new()
        .handshake::<_, Bytes>(io);
    let srv = srv.compat();

    client.write_all(b"PRI *")
        .await
        .expect("write");
    drop(client);

    srv.await.expect_err("should error");
}

#[runtime::test]
async fn request_without_authority() {
    let _ = env_logger::try_init();
    let (io, client) = mock::new();
    let io = io.compat();

    let client = client
        .assert_server_handshake()
        .recv_settings()
        .send_frame(
            frames::headers(1)
                .request("GET", "/just-a-path")
                .scheme("http")
                .eos()
        )
        .recv_frame(frames::headers(1).response(200).eos())
        .close();

    let srv = async {
        let mut srv = server::handshake(io)
            .compat()
            .await
            .expect("handshake");
        let mut srv = srv.compat();

        let (req, mut stream) = srv.next().await.unwrap().unwrap();

        assert_eq!(req.uri().path(), "/just-a-path");

        let rsp = Response::new(());
        stream.send_response(rsp, true).unwrap();

        srv.get_mut()
            .close()
            .compat()
            .await
            .unwrap();
    };

    join!(srv, client);
}
