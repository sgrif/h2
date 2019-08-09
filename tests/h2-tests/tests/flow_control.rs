#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await)]

use futures::compat::*;
use futures::join;
// use futures::prelude::*;
use h2_support::prelude::*;

// In this case, the stream & connection both have capacity, but capacity is not
// explicitly requested.
#[runtime::test]
async fn send_data_without_requesting_capacity() {
    let _ = env_logger::try_init();

    let payload = [0; 1024];

    let mock = mock_io::Builder::new()
        .handshake()
        .write(&[
            // POST /
            0, 0, 16, 1, 4, 0, 0, 0, 1, 131, 135, 65, 139, 157, 41,
            172, 75, 143, 168, 233, 25, 151, 33, 233, 132,
        ])
        .write(&[
            // DATA
            0, 4, 0, 0, 1, 0, 0, 0, 1,
        ])
        .write(&payload[..])
        .write(frames::SETTINGS_ACK)
        // Read response
        .read(&[0, 0, 1, 1, 5, 0, 0, 0, 1, 0x89])
        .build();
    let mock = mock.compat();

    let (mut client, mut h2) = client::handshake(mock)
        .compat()
        .await
        .unwrap();
    let mut h2 = h2.compat();

    let request = Request::builder()
        .method(Method::POST)
        .uri("https://http2.akamai.com/")
        .body(())
        .unwrap();

    let (response, mut stream) = client.send_request(request, false).unwrap();
    let response = response.compat();

    // The capacity should be immediately allocated
    assert_eq!(stream.capacity(), 0);

    // Send the data
    stream.send_data(payload[..].into(), true).unwrap();

    // Get the response
    let resp = h2.drive(response).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    h2.await.unwrap();
}

#[runtime::test]
async fn release_capacity_sends_window_update() {
    let _ = env_logger::try_init();

    let payload = vec![0u8; 16_384];

    let (io, srv) = mock::new();
    let io = io.compat();

    let mock = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(
            frames::headers(1)
                .response(200)
        )
        .send_frame(frames::data(1, &payload[..]))
        .send_frame(frames::data(1, &payload[..]))
        .send_frame(frames::data(1, &payload[..]))
        .recv_frame(
            frames::window_update(0, 32_768)
        )
        .recv_frame(
            frames::window_update(1, 32_768)
        )
        .send_frame(frames::data(1, &payload[..]).eos())
        // gotta end the connection
        .map(drop);

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = client.send_request(request, true)
            .unwrap()
            .0;
        let req = req.compat();
        let resp = h2.drive(req).await.unwrap();

        // Get the response
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_parts().1;
        let mut body = body.compat();

        // read some body to use up window size to below half
        let buf = h2.drive(body.next()).await.unwrap();
        assert_eq!(buf.unwrap().len(), payload.len());

        let buf = h2.drive(body.next()).await.unwrap();
        assert_eq!(buf.unwrap().len(), payload.len());

        let buf = h2.drive(body.next()).await.unwrap().unwrap();
        assert_eq!(buf.len(), payload.len());
        body.get_mut().release_capacity().release_capacity(buf.len() * 2).unwrap();

        let buf = h2.drive(body.next()).await.unwrap();
        assert_eq!(buf.unwrap().len(), payload.len());

        h2.await.unwrap();
    };

    join!(h2, mock);
}

#[runtime::test]
async fn release_capacity_of_small_amount_does_not_send_window_update() {
    let _ = env_logger::try_init();

    let payload = [0; 16];

    let (io, srv) = mock::new();
    let io = io.compat();

    let mock = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(
            frames::headers(1)
                .response(200)
        )
        .send_frame(frames::data(1, &payload[..]).eos())
        .close();

    let h2 = async {
        let (mut client, h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let h2 = h2.compat();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let (req, _) = client.send_request(request, true).unwrap();
        let req = req.compat();
        let h2 = runtime::spawn(h2.map(Result::unwrap));
        let resp = req.await.unwrap();

        // Get the response
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_parts().1;
        assert!(!body.is_end_stream());
        let mut body = body.compat();

        // read the small body and then release it
        let buf = body.next().await.unwrap().unwrap();
        assert_eq!(buf.len(), 16);
        body
            .get_mut()
            .release_capacity()
            .release_capacity(buf.len())
            .unwrap();

        let buf = body.next().await;
        assert!(buf.is_none());

        h2.await;
    };

    join!(h2, mock);
}

#[runtime::test]
#[ignore]
async fn expand_window_sends_window_update() {}

#[runtime::test]
#[ignore]
async fn expand_window_calls_are_coalesced() {}

#[runtime::test]
async fn recv_data_overflows_connection_window() {
    let _ = env_logger::try_init();

    let (io, srv) = mock::new();
    let io = io.compat();

    let mock = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(
            frames::headers(1)
                .response(200)
        )
        // fill the whole window
        .send_frame(frames::data(1, vec![0u8; 16_384]))
        .send_frame(frames::data(1, vec![0u8; 16_384]))
        .send_frame(frames::data(1, vec![0u8; 16_384]))
        .send_frame(frames::data(1, vec![0u8; 16_383]))
        // this frame overflows the window!
        .send_frame(frames::data(1, vec![0u8; 128]).eos())
        // expecting goaway for the conn, not stream
        .recv_frame(frames::go_away(0).flow_control());
    // connection is ended by client

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (req, _) = client
            .send_request(request, true)
            .unwrap();
        let req = req.compat();
        let resp = h2.drive(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_parts().1;
        let body = body.compat();
        let err = body.try_concat()
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "protocol error: flow-control protocol violated"
        );

        // client should see a flow control error
        let err = h2.await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "protocol error: flow-control protocol violated"
        );
    };
    // Order is important here, if we poll the client first the server will
    // cause a flow control violation before a settings ack is received.
    join!(mock, h2);
}

#[runtime::test]
async fn recv_data_overflows_stream_window() {
    // this tests for when streams have smaller windows than their connection
    let _ = env_logger::try_init();

    let (io, srv) = mock::new();
    let io = io.compat();

    let mock = srv.assert_client_handshake()
        .ignore_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(
            frames::headers(1)
                .response(200)
        )
        // fill the whole window
        .send_frame(frames::data(1, vec![0u8; 16_384]))
        // this frame overflows the window!
        .send_frame(frames::data(1, &[0; 16][..]).eos())
        .recv_frame(frames::reset(1).flow_control())
        .close();

    let h2 = async {
        let (mut client, mut conn) = client::Builder::new()
            .initial_window_size(16_384)
            .handshake::<_, Bytes>(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let request = Request::builder()
            .method(Method::GET)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let req = client
            .send_request(request, true)
            .unwrap()
            .0;
        let req = req.compat();

        let resp = conn.drive(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_parts().1;
        let body = body.compat();
        let err = body.try_concat().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "protocol error: flow-control protocol violated"
        );

        conn.await.unwrap();
    };
    join!(h2, mock);
}

#[runtime::test]
#[ignore]
async fn recv_window_update_causes_overflow() {
    // A received window update causes the window to overflow.
}

#[runtime::test]
async fn stream_error_release_connection_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        // we're sending the wrong content-length
        .send_frame(
            frames::headers(1)
                .response(200)
                .field("content-length", &*(16_384 * 3).to_string())
        )
        .send_frame(frames::data(1, vec![0; 16_384]))
        .send_frame(frames::data(1, vec![0; 16_384]))
        .send_frame(frames::data(1, vec![0; 10]).eos())
        // mismatched content-length is a protocol error
        .recv_frame(frames::reset(1).protocol_error())
        // but then the capacity should be released automatically
        .recv_frame(frames::window_update(0, 16_384 * 2 + 10))
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(()).unwrap();

        let (req, _) = client.send_request(request, true).unwrap();
        let req = req.compat();
        let resp = conn.drive(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_parts().1;
        let mut cap = body.release_capacity().clone();
        let mut body = body.compat();
        let to_release = 16_384 * 2;
        let mut should_recv_bytes = to_release;
        let mut should_recv_frames = 2i32;
        let err = body.try_for_each(move |bytes| {
            should_recv_bytes -= bytes.len();
            should_recv_frames -= 1;
            if should_recv_bytes == 0 {
                assert_eq!(should_recv_bytes, 0);
            }

            future::ready(Ok(()))
        })
        .await
        .expect_err("body");

        assert_eq!(
            err.to_string(),
            "protocol error: unspecific protocol error detected"
        );
        cap.release_capacity(to_release).expect("release_capacity");

        conn.await.unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
async fn stream_close_by_data_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Send request
        let (resp1, mut s1) = client.send_request(request, false).unwrap();

        // This effectively reserves the entire connection window
        s1.reserve_capacity(window_size);

        // The capacity should be immediately available as nothing else is
        // happening on the stream.
        assert_eq!(s1.capacity(), window_size);

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Create a second stream
        let (resp2, mut s2) = client.send_request(request, false).unwrap();

        // Request capacity
        s2.reserve_capacity(5);

        // There should be no available capacity (as it is being held up by
        // the previous stream
        assert_eq!(s2.capacity(), 0);

        // Closing the previous stream by sending an empty data frame will
        // release the capacity to s2
        s1.send_data("".into(), true).unwrap();

        // The capacity should be available
        assert_eq!(s2.capacity(), 5);

        // Send the frame
        s2.send_data("hello".into(), true).unwrap();

        // Drive both streams to prevent the handles from being dropped
        // (which will send a RST_STREAM) before the connection is closed.
        let resp1 = resp1.compat();
        h2.drive(resp1).await.unwrap();
        let resp2 = resp2.compat();
        h2.drive(resp2).await.unwrap();
        h2.await.unwrap();
    };

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
        .send_frame(frames::headers(1).response(200))
        .recv_frame(frames::headers(3).request("POST", "https://http2.akamai.com/"))
        .send_frame(frames::headers(3).response(200))
        .recv_frame(frames::data(1, &b""[..]).eos())
        .recv_frame(frames::data(3, &b"hello"[..]).eos())
        .close();

    join!(h2, srv);
}

#[runtime::test]
async fn stream_close_by_trailers_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Send request
        let (resp1, mut s1) = client.send_request(request, false).unwrap();

        // This effectively reserves the entire connection window
        s1.reserve_capacity(window_size);

        // The capacity should be immediately available as nothing else is
        // happening on the stream.
        assert_eq!(s1.capacity(), window_size);

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        // Create a second stream
        let (resp2, mut s2) = client.send_request(request, false).unwrap();

        // Request capacity
        s2.reserve_capacity(5);

        // There should be no available capacity (as it is being held up by
        // the previous stream
        assert_eq!(s2.capacity(), 0);

        // Closing the previous stream by sending a trailers frame will
        // release the capacity to s2
        s1.send_trailers(Default::default()).unwrap();

        // The capacity should be available
        assert_eq!(s2.capacity(), 5);

        // Send the frame
        s2.send_data("hello".into(), true).unwrap();

        // Drive both streams to prevent the handles from being dropped
        // (which will send a RST_STREAM) before the connection is closed.
       let resp1 = resp1.compat();
       h2.drive(resp1).await.unwrap();
       let resp2 = resp2.compat();
       h2.drive(resp2).await.unwrap();
       h2.await.unwrap();
    };

    let srv = srv.assert_client_handshake()
        // Get the first frame
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://http2.akamai.com/")
        )
        .send_frame(frames::headers(1).response(200))
        .recv_frame(
            frames::headers(3)
                .request("POST", "https://http2.akamai.com/")
        )
        .send_frame(frames::headers(3).response(200))
        .recv_frame(frames::headers(1).eos())
        .recv_frame(frames::data(3, &b"hello"[..]).eos())
        .close();

    join!(h2, srv);
}

#[runtime::test]
async fn stream_close_by_send_reset_frame_releases_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(frames::headers(1).response(200))
        .send_frame(frames::data(1, vec![0; 16_384]))
        .send_frame(frames::data(1, vec![0; 16_384]).eos())
        .recv_frame(frames::window_update(0, 16_384 * 2))
        .recv_frame(
            frames::headers(3)
                .request("GET", "https://http2.akamai.com/")
                .eos()
        )
        .send_frame(frames::headers(3).response(200).eos())
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
        let (resp, _) = client.send_request(request, true).unwrap();

        let resp = resp.compat();
        conn.drive(resp).await.unwrap();
        // ^-- ignore the response body

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();
        let (resp, _) = client.send_request(request, true).unwrap();
        let resp = resp.compat();
        conn.drive(resp).await.unwrap();

        conn.await.unwrap();
    };

    join!(srv, client);
}

#[runtime::test]
#[ignore]
async fn stream_close_by_recv_reset_frame_releases_capacity() {}

#[runtime::test]
async fn recv_window_update_on_stream_closed_by_data_frame() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();
        let response = response.compat();
        let response = h2.drive(response).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Send a data frame, this will also close the connection
        stream.send_data("hello".into(), true).unwrap();

        // keep `stream` from being dropped in order to prevent
        // it from sending an RST_STREAM frame.
        //
        // i know this is kind of evil, but it's necessary to
        // ensure that the stream is closed by the EOS frame,
        // and not by the RST_STREAM.
        std::mem::forget(stream);

        h2.await.unwrap();
    };

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frames::headers(1).request("POST", "https://http2.akamai.com/"))
        .send_frame(frames::headers(1).response(200))
        .recv_frame(frames::data(1, "hello").eos())
        .send_frame(frames::window_update(1, 5))
        .map(drop);

    join!(h2, srv);
}

#[runtime::test]
async fn reserved_capacity_assigned_in_multi_window_updates() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        // Consume the capacity
        let payload = vec![0; frame::DEFAULT_INITIAL_WINDOW_SIZE as usize];
        stream.send_data(payload.into(), false).unwrap();

        // Reserve more data than we want
        stream.reserve_capacity(10);

        let hack_because_task_current_used_in_h2 = util::wait_for_capacity(stream, 5)
            .unit_error()
            .compat()
            .compat();
        let mut stream = h2.drive(hack_because_task_current_used_in_h2)
            .await;
        let mut stream = stream.unwrap();
        stream.send_data("hello".into(), false).unwrap();
        stream.send_data("world".into(), true).unwrap();

        let response = response.compat();
        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        h2.await.unwrap();
    };

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://http2.akamai.com/")
        )
        .recv_frame(frames::data(1, vec![0u8; 16_384]))
        .recv_frame(frames::data(1, vec![0u8; 16_384]))
        .recv_frame(frames::data(1, vec![0u8; 16_384]))
        .recv_frame(frames::data(1, vec![0u8; 16_383]))
        .idle_ms(100)
        // Increase the connection window
        .send_frame(
            frames::window_update(0, 10))
        // Incrementally increase the stream window
        .send_frame(
            frames::window_update(1, 4))
        .idle_ms(50)
        .send_frame(
            frames::window_update(1, 1))
        // Receive first chunk
        .recv_frame(frames::data(1, "hello"))
        .send_frame(
            frames::window_update(1, 5))
        // Receive second chunk
        .recv_frame(
            frames::data(1, "world").eos())
        .send_frame(
            frames::headers(1)
                .response(204)
                .eos()
        )
        /*
        .recv_frame(frames::data(1, "hello").eos())
        .send_frame(frames::window_update(1, 5))
        */
        .map(drop);

    join!(h2, srv);
}

// #[runtime::test]
// async fn connection_notified_on_released_capacity() {
//     use futures::channel::oneshot;
//     use futures::channel::mpsc;
//     use std::thread;

//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//     let io = io.compat();

//     // We're going to run the connection on a thread in order to isolate task
//     // notifications. This test is here, in part, to ensure that the connection
//     // receives the appropriate notifications to send out window updates.

//     let (mut tx, mut rx) = mpsc::channel(2);

//     // Because threading is fun
//     let (settings_tx, settings_rx) = oneshot::channel();

//     let th1 = runtime::spawn(async move {
//         let mut srv = srv.assert_client_handshake()
//             .recv_settings()
//             .await;
//         settings_tx.send(()).unwrap();
//         // Get the first request
//         srv.recv_frame(
//                 frames::headers(1)
//                     .request("GET", "https://example.com/a")
//                     .eos())
//             .await;
//         // Get the second request
//         srv.recv_frame(
//                 frames::headers(3)
//                     .request("GET", "https://example.com/b")
//                     .eos())
//             .await;
//         // Send the first response
//         srv.send_frame(frames::headers(1).response(200)).await;
//         // Send the second response
//         srv.send_frame(frames::headers(3).response(200)).await;

//         // Fill the connection window
//         srv.send_frame(frames::data(1, vec![0u8; 16_384]).eos()).await;
//         util::idle_ms(100).await;
//         srv.send_frame(frames::data(3, vec![0u8; 16_384]).eos()).await;

//         // The window update is sent
//         srv.recv_frame(frames::window_update(0, 16_384)).await;
//         srv.close().await;
//     });

//     let th2 = runtime::spawn(async move {
//         let (mut client, mut h2) = client::handshake(io)
//             .compat()
//             .await
//             .unwrap();
//         let mut h2 = h2.compat();

//         h2.drive(settings_rx).await.unwrap();

//         let request = Request::get("https://example.com/a").body(()).unwrap();

//         tx.send(client.send_request(request, true).unwrap())
//             .await
//             .unwrap();

//         let request = Request::get("https://example.com/b").body(()).unwrap();

//         tx.send(client.send_request(request, true).unwrap())
//             .await
//             .unwrap();

//         // Run the connection to completion
//         h2.await.unwrap();
//     });

//     // Get the two requests
//     let (a, _) = rx.next().await.unwrap();
//     let (b, _) = rx.next().await.unwrap();

//     // Get the first response
//     let a = a.compat();
//     let response = a.await.unwrap();
//     assert_eq!(response.status(), StatusCode::OK);
//     let (_, mut a) = response.into_parts();

//     // Get the next chunk
//     let mut a = a.compat();
//     let chunk = a.next().await.unwrap();
//     assert_eq!(16_384, chunk.unwrap().len());

//     // Get the second response
//     let b = b.compat();
//     let response = b.await.unwrap();
//     assert_eq!(response.status(), StatusCode::OK);
//     let (_, mut b) = response.into_parts();

//     // Get the next chunk
//     let mut b = b.compat();
//     let chunk = b.next().await.unwrap();
//     assert_eq!(16_384, chunk.unwrap().len());

//     // Wait a bit
//     thread::sleep(Duration::from_millis(100));

//     // Release the capacity
//     a.get_mut().release_capacity().release_capacity(16_384).unwrap();

//     // FIXME: This test is flaky. In this order we sometimes fail because we
//     // never got a settings ack. If we switch the order
//     // we sometimes hang forever.
//     join!(th2, th1);

//     // Explicitly drop this after the joins so that the capacity doesn't get
//     // implicitly released before.
//     drop(b);
// }

#[runtime::test]
async fn recv_settings_removes_available_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(0));

    let srv = srv.assert_client_handshake_with_settings(settings)
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://http2.akamai.com/")
        )
        .idle_ms(100)
        .send_frame(frames::window_update(0, 11))
        .send_frame(frames::window_update(1, 11))
        .recv_frame(frames::data(1, "hello world").eos())
        .send_frame(
            frames::headers(1)
                .response(204)
                .eos()
        )
        .close();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(()).unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let hack_because_task_current_used_in_h2 = util::wait_for_capacity(stream, 5)
            .unit_error()
            .compat()
            .compat();
        let mut stream = h2.drive(hack_because_task_current_used_in_h2)
            .await
            .unwrap();
        assert_eq!(stream.capacity(), 11);

        stream.send_data("hello world".into(), true).unwrap();

        let response = response.compat();
        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        // Hold on to the `client` handle to avoid sending a GO_AWAY frame.
        h2.await.unwrap();
    };

    join!(h2, srv);
}

#[runtime::test]
async fn recv_settings_keeps_assigned_capacity() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let (sent_settings, sent_settings_rx) = futures::channel::oneshot::channel();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://http2.akamai.com/")
        )
        .send_frame(frames::settings().initial_window_size(64))
        .then_notify(sent_settings)
        .recv_frame(frames::data(1, "hello world").eos())
        .send_frame(
            frames::headers(1)
                .response(204)
                .eos()
        )
        .close();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(()).unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let hack_because_task_current_used_in_h2 = util::wait_for_capacity(stream, 11)
            .unit_error()
            .compat()
            .compat();
        let mut stream = h2.drive(hack_because_task_current_used_in_h2)
            .await
            .unwrap();
        h2.drive(sent_settings_rx).await.unwrap();
        stream.send_data("hello world".into(), true).unwrap();
        let response = response.compat();
        let resp = h2.drive(response).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        h2.await.unwrap();
    };

    join!(srv, h2);
}

#[runtime::test]
async fn recv_no_init_window_then_receive_some_init_window() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let mut settings = frame::Settings::default();
    settings.set_initial_window_size(Some(0));

    let srv = srv.assert_client_handshake_with_settings(settings)
        .recv_settings()
        .recv_frame(
            frames::headers(1)
                .request("POST", "https://http2.akamai.com/")
        )
        .idle_ms(100)
        .send_frame(frames::settings().initial_window_size(10))
        .recv_frame(frames::data(1, "hello worl"))
        .idle_ms(100)
        .send_frame(frames::settings().initial_window_size(11))
        .recv_frame(frames::data(1, "d").eos())
        .send_frame(
            frames::headers(1)
                .response(204)
                .eos()
        )
        .close();

    let h2 = async {
        let (mut client, mut h2) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut h2 = h2.compat();

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://http2.akamai.com/")
            .body(()).unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();

        stream.reserve_capacity(11);

        let hack_because_task_current_used_in_h2 = util::wait_for_capacity(stream, 11)
            .unit_error()
            .compat()
            .compat();
        let mut stream = h2.drive(hack_because_task_current_used_in_h2)
            .await
            .unwrap();
        assert_eq!(stream.capacity(), 11);

        stream.send_data("hello world".into(), true).unwrap();

        let response = response.compat();
        let response = h2.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Wait for the connection to close
        // Hold on to the `client` handle to avoid sending a GO_AWAY frame.
        h2.await.unwrap();
    };

    join!(srv, h2);
}

// #[runtime::test]
// async fn settings_lowered_capacity_returns_capacity_to_connection() {
//     use futures::channel::mpsc;

//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//     let io = io.compat();
//     let (mut tx1, mut rx1) = mpsc::unbounded();
//     let (mut tx2, mut rx2) = mpsc::unbounded();

//     let window_size = frame::DEFAULT_INITIAL_WINDOW_SIZE as usize;

//     // Spawn the server on a thread
//     let th1 = runtime::spawn(async move {
//         let mut srv = srv.assert_client_handshake()
//             .recv_settings()
//             .await;

//         tx1.send(()).await.unwrap();

//         srv.recv_frame(
//                 frames::headers(1)
//                     .request("POST", "https://example.com/one")
//             )
//             .await;
//         srv.recv_frame(
//                 frames::headers(3)
//                     .request("POST", "https://example.com/two")
//             ).await;
//         util::idle_ms(200).await;
//         // Remove all capacity from streams
//         srv.send_frame(frames::settings().initial_window_size(0)).await;

//         // Let stream 3 make progress
//         srv.send_frame(frames::window_update(3, 11)).await;
//         srv.recv_frame(frames::data(3, "hello world").eos()).await;

//         // Wait to get notified
//         let _ = rx2.next().await.unwrap();

//         util::idle_ms(500).await;

//         // Reset initial window size
//         srv.send_frame(frames::settings().initial_window_size(window_size as u32)).await;

//         // Get data from first stream
//         srv.recv_frame(frames::data(1, "hello world").eos()).await;

//         // Send responses
//         srv.send_frame(
//             frames::headers(1)
//                 .response(204)
//                 .eos()
//         ).await;
//         srv.send_frame(
//             frames::headers(3)
//                 .response(204)
//                 .eos()
//         ).await;
//         srv.close().await;
//     });

//     let (mut client, mut h2) = client::handshake(io)
//         .compat()
//         .await
//         .unwrap();
//     let mut h2 = h2.compat();

//     // Wait for server handshake to complete.
//     h2.drive(rx1.next()).await.unwrap();

//     let request = Request::post("https://example.com/one")
//         .body(()).unwrap();

//     let (resp1, mut stream1) = client.send_request(request, false).unwrap();
//     let resp1 = resp1.compat();

//     let request = Request::post("https://example.com/two")
//         .body(()).unwrap();

//     let (resp2, mut stream2) = client.send_request(request, false).unwrap();
//     let resp2 = resp2.compat();

//     // Reserve capacity for stream one, this will consume all connection level
//     // capacity
//     stream1.reserve_capacity(window_size);
//     let stream1 = util::wait_for_capacity(stream1, window_size)
//         .unit_error()
//         .compat()
//         .compat();
//     let stream1 = h2.drive(stream1)
//         .await
//         .unwrap();

//     // Now, wait for capacity on the other stream
//     stream2.reserve_capacity(11);
//     let mut stream2 = util::wait_for_capacity(stream2, 11)
//         .unit_error()
//         .compat()
//         .compat();
//     let mut stream2 = h2.drive(stream2)
//         .await
//         .unwrap();

//     // Send data on stream 2
//     stream2.send_data("hello world".into(), true).unwrap();

//     tx2.send(()).await.unwrap();

//     // Wait for capacity on stream 1
//     let mut stream1 = util::wait_for_capacity(stream1, 11)
//         .unit_error()
//         .compat()
//         .compat();
//     let mut stream1 = h2.drive(stream1)
//         .await
//         .unwrap();

//     stream1.send_data("hello world".into(), true).unwrap();

//     // Wait for responses..
//     let resp = h2.drive(resp1).await.unwrap();
//     assert_eq!(resp.status(), StatusCode::NO_CONTENT);

//     let resp = h2.drive(resp2).await.unwrap();
//     assert_eq!(resp.status(), StatusCode::NO_CONTENT);

//     let (_, res) = join!(th1, h2);
//     res.unwrap();
// }

#[runtime::test]
async fn client_increase_target_window_size() {
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frames::window_update(0, (2 << 20) - 65_535))
        .close();

    let client = async {
        let (_client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        conn.set_target_window_size(2 << 20);
        let mut conn = conn.compat();
        conn.await.unwrap();
    };

    join!(srv, client);
}

// #[runtime::test]
// async fn increase_target_window_size_after_using_some() {
//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//
//     let srv = srv.assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(
//             frames::headers(1)
//                 .request("GET", "https://http2.akamai.com/")
//                 .eos()
//         )
//         .send_frame(frames::headers(1).response(200))
//         .send_frame(frames::data(1, vec![0; 16_384]).eos())
//         .recv_frame(frames::window_update(0, (2 << 20) - 65_535))
//         .close();
//
//     let client = client::handshake(io).unwrap()
//         .and_then(|(mut client, conn)| {
//             let request = Request::builder()
//                 .uri("https://http2.akamai.com/")
//                 .body(()).unwrap();
//
//             let res = client.send_request(request, true).unwrap().0;
//
//             conn.drive(res)
//         })
//         .and_then(|(mut conn, res)| {
//             conn.set_target_window_size(2 << 20);
//             // drive an empty future to allow the WINDOW_UPDATE
//             // to go out while the response capacity is still in use.
//             let mut yielded = false;
//             conn.drive(futures::future::poll_fn(move || {
//                 if yielded {
//                     Ok::<_, ()>(().into())
//                 } else {
//                     yielded = true;
//                     futures::task::current().notify();
//                     Ok(futures::Async::NotReady)
//                 }
//             }))
//                 .map(move |(c, _)| (c, res))
//         })
//         .and_then(|(conn, res)| {
//             conn.drive(res.into_body().concat2())
//                 .and_then(|(c, _)| c.expect("client"))
//         });
//
//     srv.join(client).wait().unwrap();
// }
//
// #[runtime::test]
// async fn decrease_target_window_size() {
//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//
//     let srv = srv.assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(
//             frames::headers(1)
//                 .request("GET", "https://http2.akamai.com/")
//                 .eos()
//         )
//         .send_frame(frames::headers(1).response(200))
//         .send_frame(frames::data(1, vec![0; 16_384]))
//         .send_frame(frames::data(1, vec![0; 16_384]))
//         .send_frame(frames::data(1, vec![0; 16_384]))
//         .send_frame(frames::data(1, vec![0; 16_383]).eos())
//         .recv_frame(frames::window_update(0, 16_384))
//         .close();
//
//     let client = client::handshake(io).unwrap()
//         .and_then(|(mut client, mut conn)| {
//             conn.set_target_window_size(16_384 * 2);
//
//             let request = Request::builder()
//                 .uri("https://http2.akamai.com/")
//                 .body(()).unwrap();
//             let (resp, _) = client.send_request(request, true).unwrap();
//             conn.drive(resp.expect("response")).map(|c| (c, client))
//         })
//         .and_then(|((mut conn, res), client)| {
//             conn.set_target_window_size(16_384);
//             let mut body = res.into_parts().1;
//             let mut cap = body.release_capacity().clone();
//
//             conn.drive(body.concat2().expect("concat"))
//                 .map(|c| (c, client))
//                 .and_then(move |((conn, bytes), client)| {
//                     assert_eq!(bytes.len(), 65_535);
//                     cap.release_capacity(bytes.len()).unwrap();
//                     conn.expect("conn").map(|c| (c, client))
//                 })
//         });
//
//     srv.join(client).wait().unwrap();
// }
//
// #[runtime::test]
// async fn server_target_window_size() {
//     let _ = env_logger::try_init();
//     let (io, client) = mock::new();
//
//     let client = client.assert_server_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(frames::window_update(0, (2 << 20) - 65_535))
//         .close();
//
//     let srv = server::handshake(io).unwrap()
//         .and_then(|mut conn| {
//             conn.set_target_window_size(2 << 20);
//             conn.into_future().unwrap()
//         });
//
//     srv.join(client).wait().unwrap();
// }
//
// #[runtime::test]
// async fn recv_settings_increase_window_size_after_using_some() {
//     // See https://github.com/hyperium/h2/issues/208
//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//
//     let new_win_size = 16_384 * 4; // 1 bigger than default
//     let srv = srv.assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(
//             frames::headers(1)
//                 .request("POST", "https://http2.akamai.com/")
//         )
//         .recv_frame(frames::data(1, vec![0; 16_384]))
//         .recv_frame(frames::data(1, vec![0; 16_384]))
//         .recv_frame(frames::data(1, vec![0; 16_384]))
//         .recv_frame(frames::data(1, vec![0; 16_383]))
//         .send_frame(
//             frames::settings()
//                 .initial_window_size(new_win_size as u32)
//         )
//         .recv_frame(frames::settings_ack())
//         .send_frame(frames::window_update(0, 1))
//         .recv_frame(frames::data(1, vec![0; 1]).eos())
//         .send_frame(frames::headers(1).response(200).eos())
//         .close();
//
//     let client = client::handshake(io).unwrap()
//         .and_then(|(mut client, conn)| {
//             let request = Request::builder()
//                 .method("POST")
//                 .uri("https://http2.akamai.com/")
//                 .body(()).unwrap();
//             let (resp, mut req_body) = client.send_request(request, false).unwrap();
//             req_body.send_data(vec![0; new_win_size].into(), true).unwrap();
//             conn.drive(resp.expect("response")).map(|c| (c, client))
//         })
//         .and_then(|((conn, _res), client)| {
//             conn.expect("client").map(|c| (c, client))
//         });
//
//     srv.join(client).wait().unwrap();
// }
//
// #[runtime::test]
// async fn reserve_capacity_after_peer_closes() {
//     // See https://github.com/hyperium/h2/issues/300
//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//
//     let srv = srv.assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(
//             frames::headers(1)
//                 .request("POST", "https://http2.akamai.com/")
//         )
//         // close connection suddenly
//         .close();
//
//     let client = client::handshake(io).unwrap()
//         .and_then(|(mut client, conn)| {
//             let request = Request::builder()
//                 .method("POST")
//                 .uri("https://http2.akamai.com/")
//                 .body(()).unwrap();
//             let (resp, req_body) = client.send_request(request, false).unwrap();
//             conn.drive(resp.then(move |result| {
//                 assert!(result.is_err());
//                 Ok::<_, ()>(req_body)
//             }))
//         })
//         .and_then(|(conn, mut req_body)| {
//             // As stated in #300, this would panic because the connection
//             // had already been closed.
//             req_body.reserve_capacity(1);
//             conn.expect("client")
//         });
//
//     srv.join(client).wait().expect("wait");
// }
//
// #[runtime::test]
// async fn reset_stream_waiting_for_capacity() {
//     // This tests that receiving a reset on a stream that has some available
//     // connection-level window reassigns that window to another stream.
//     let _ = env_logger::try_init();
//
//     let (io, srv) = mock::new();
//
//     let srv = srv
//         .assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(frames::headers(1).request("GET", "http://example.com/"))
//         .recv_frame(frames::headers(3).request("GET", "http://example.com/"))
//         .recv_frame(frames::headers(5).request("GET", "http://example.com/"))
//         .recv_frame(frames::data(1, vec![0; 16384]))
//         .recv_frame(frames::data(1, vec![0; 16384]))
//         .recv_frame(frames::data(1, vec![0; 16384]))
//         .recv_frame(frames::data(1, vec![0; 16383]).eos())
//         .send_frame(frames::headers(1).response(200))
//         // Assign enough connection window for stream 3...
//         .send_frame(frames::window_update(0, 1))
//         // but then reset it.
//         .send_frame(frames::reset(3))
//         // 5 should use that window instead.
//         .recv_frame(frames::data(5, vec![0; 1]).eos())
//         .send_frame(frames::headers(5).response(200))
//         .close()
//         ;
//
//     fn request() -> Request<()> {
//         Request::builder()
//             .uri("http://example.com/")
//             .body(())
//             .unwrap()
//     }
//
//     let client = client::Builder::new()
//         .handshake::<_, Bytes>(io)
//         .expect("handshake")
//         .and_then(move |(mut client, conn)| {
//             let (req1, mut send1) = client.send_request(
//                 request(), false).unwrap();
//             let (req2, mut send2) = client.send_request(
//                 request(), false).unwrap();
//             let (req3, mut send3) = client.send_request(
//                 request(), false).unwrap();
//             // Use up the connection window.
//             send1.send_data(vec![0; 65535].into(), true).unwrap();
//             // Queue up for more connection window.
//             send2.send_data(vec![0; 1].into(), true).unwrap();
//             // .. and even more.
//             send3.send_data(vec![0; 1].into(), true).unwrap();
//             conn.expect("h2")
//                 .join(req1.expect("req1"))
//                 .join(req2.then(|r| Ok(r.unwrap_err())))
//                 .join(req3.expect("req3"))
//         });
//
//
//     client.join(srv).wait().unwrap();
// }
//
//
// #[runtime::test]
// async fn data_padding() {
//     let _ = env_logger::try_init();
//     let (io, srv) = mock::new();
//
//     let mut body = Vec::new();
//     body.push(5);
//     body.extend_from_slice(&[b'z'; 100][..]);
//     body.extend_from_slice(&[b'0'; 5][..]);
//
//     let srv = srv.assert_client_handshake()
//         .unwrap()
//         .recv_settings()
//         .recv_frame(
//             frames::headers(1)
//                 .request("GET", "http://example.com/")
//                 .eos()
//         )
//         .send_frame(
//             frames::headers(1)
//                 .response(200)
//                 .field("content-length", 100)
//         )
//         .send_frame(
//             frames::data(1, body)
//                 .padded()
//                 .eos()
//         )
//         .close();
//
//     let h2 = client::handshake(io)
//         .expect("handshake")
//         .and_then(|(mut client, conn)| {
//             let request = Request::builder()
//                 .method(Method::GET)
//                 .uri("http://example.com/")
//                 .body(())
//                 .unwrap();
//
//             // first request is allowed
//             let (response, _) = client.send_request(request, true).unwrap();
//             let fut = response
//                 .and_then(|resp| {
//                     assert_eq!(resp.status(), StatusCode::OK);
//                     let body = resp.into_body();
//                     body.concat2()
//                 })
//                 .map(|bytes| {
//                     assert_eq!(bytes.len(), 100);
//                 });
//             conn
//                 .expect("client")
//                 .join(fut.expect("response"))
//         });
//
//     h2.join(srv).wait().expect("wait");
// }
