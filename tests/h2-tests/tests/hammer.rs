#![deny(warnings)]
#![allow(unused_mut)] // FIXME: Remove once std::future port is finished
#![feature(async_await, async_closure)]

use h2_support::prelude::*;
use futures::compat::*;

use runtime::net::{TcpListener, TcpStream};
use std::{net::SocketAddr, sync::{atomic::{AtomicUsize, Ordering}, Arc}};

async fn serve<F>(
    addr: SocketAddr,
    mk_data: F,
) -> Arc<AtomicUsize>
where
    F: Fn() -> Bytes,
    F: Send + Sync + 'static,
{
    let mk_data = Arc::new(mk_data);

    let mut listener = TcpListener::bind(addr).unwrap();
    let reqs = Arc::new(AtomicUsize::new(0));
    let out = reqs.clone();
    let _ = runtime::spawn(async move {
        listener.incoming().for_each_concurrent(None, |socket| {
            let socket = socket.unwrap();
            let socket = socket.compat();
            let reqs = reqs.clone();
            let mk_data = mk_data.clone();
            runtime::spawn(async move {
                server::handshake(socket)
                    .compat()
                    .await?
                    .compat()
                    .try_for_each(move |(_, mut respond)| {
                        reqs.fetch_add(1, Ordering::Release);
                        let response = Response::builder()
                            .status(StatusCode::OK)
                            .body(())
                            .unwrap();
                        let mut send = respond.send_response(response, false)
                            .unwrap();
                        send.send_data(mk_data(), true).unwrap();
                        future::ready(Ok(()))
                    }).await?;
                Ok::<_, h2::Error>(())
            }).map(Result::unwrap)
        }).await;
    });

    out
}

#[runtime::test]
async fn hammer_client_concurrency() {
    // This reproduces issue #326.
    const N: usize = 5000;

    let addr = SocketAddr::from(([127, 0, 0, 1], 2345));
    let rsps = Arc::new(AtomicUsize::new(0));
    let server_reqs = serve(addr.clone(), || Bytes::from_static(b"hello world!"))
        .await;

    for i in 0..N {
        println!("sending {}", i);
        let tcp = TcpStream::connect(&addr).await.unwrap();
        let tcp = tcp.compat();

        let (mut client, h2) = client::handshake(tcp)
            .compat()
            .await
            .unwrap();
        let h2 = h2.compat();

        let request = Request::builder()
            .uri("https://http2.akamai.com/")
            .body(())
            .unwrap();

        let (response, mut stream) = client.send_request(request, false).unwrap();
        stream.send_trailers(HeaderMap::new()).unwrap();

        let _ = runtime::spawn(h2.map(Result::unwrap));
        let response = response.compat();
        let (_, mut body) = response.await.unwrap().into_parts();
        let mut body = body.compat();
        while body.next().await.is_some() {
        }
        body.get_mut()
            .trailers()
            .compat()
            .await
            .unwrap();
        rsps.fetch_add(1, Ordering::Release);

        println!("...done");
    }

    println!("all done");

    assert_eq!(N, rsps.load(Ordering::Acquire));
    assert_eq!(N, server_reqs.load(Ordering::Acquire));
}
