use h2;

use bytes::Bytes;
use futures::prelude::*;
use futures::ready;
use std::pin::Pin;
use std::task::*;
use string::{String, TryFrom};

pub fn byte_str(s: &str) -> String<Bytes> {
    String::try_from(Bytes::from(s)).unwrap()
}

pub async fn idle_ms(ms: usize) {
    use futures::channel::oneshot;
    use std::thread;
    use std::time::Duration;

    // This is terrible... but oh well
    let (tx, rx) = oneshot::channel();

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms as u64));
        tx.send(()).unwrap();
    });

    rx.await.unwrap();
}

pub fn wait_for_capacity(stream: h2::SendStream<Bytes>, target: usize) -> WaitForCapacity {
    WaitForCapacity {
        stream: Some(stream),
        target: target,
    }
}

pub struct WaitForCapacity {
    stream: Option<h2::SendStream<Bytes>>,
    target: usize,
}

impl WaitForCapacity {
    fn stream(&mut self) -> &mut h2::SendStream<Bytes> {
        self.stream.as_mut().unwrap()
    }
}

impl Future for WaitForCapacity {
    type Output = h2::SendStream<Bytes>;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context) -> Poll<Self::Output> {
        ready!(poll_01_to_03(self.stream().poll_capacity()))
            .unwrap();

        let act = self.stream().capacity();

        if act >= self.target {
            Poll::Ready(self.stream.take().unwrap())
        } else {
            Poll::Pending
        }
    }
}

pub(crate) fn poll_01_to_03<T, E>(old: futures01::Poll<T, E>) -> Poll<Result<T, E>> {
    match old {
        Ok(futures01::Async::Ready(x)) => Poll::Ready(Ok(x)),
        Ok(futures01::Async::NotReady) => Poll::Pending,
        Err(e) => Poll::Ready(Err(e)),
    }
}
