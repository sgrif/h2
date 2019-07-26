#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await)]

use h2_support::prelude::*;
use futures::join;
use futures::compat::*;

#[runtime::test]
async fn write_continuation_frames() {
    // An invalid dependency ID results in a stream level error. The hpack
    // payload should still be decoded.
    let _ = env_logger::try_init();
    let (io, srv) = mock::new();
    let io = io.compat();

    let large = build_large_headers();

    // Build the large request frame
    let frame = large.iter().fold(
        frames::headers(1).request("GET", "https://http2.akamai.com/"),
        |frame, &(name, ref value)| frame.field(name, &value[..]));

    let srv = srv.assert_client_handshake()
        .recv_settings()
        .recv_frame(frame.eos())
        .send_frame(
            frames::headers(1)
                .response(204)
                .eos(),
        )
        .close();

    let client = async {
        let (mut client, mut conn) = client::handshake(io)
            .compat()
            .await
            .unwrap();
        let mut conn = conn.compat();

        let mut request = Request::builder();
        request.uri("https://http2.akamai.com/");

        for &(name, ref value) in &large {
            request.header(name, &value[..]);
        }

        let request = request
            .body(())
            .unwrap();

        let (response, _) = client
            .send_request(request, true)
            .unwrap();
        let response = response.compat();

        let response = conn.drive(response).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        conn.await.unwrap();
    };

    join!(client, srv);
}
