use h2;

use futures::prelude::*;
use super::string::{String, TryFrom};
use bytes::Bytes;
use std::task::*;
use std::pin::Pin;

pub fn byte_str(s: &str) -> String<Bytes> {
    String::try_from(Bytes::from(s)).unwrap()
}

pub async fn yield_once() {
    futures::pending!();
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

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
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
