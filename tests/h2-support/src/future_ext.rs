use futures::prelude::*;
use pin_utils::*;
use std::task::*;
use std::pin::Pin;

use std::fmt;

/// Future extension helpers that are useful for tests
pub trait FutureExt: Future {
    /// Panic on error
    fn unwrap(self) -> Unwrap<Self>
    where
        Self: Sized,
        Unwrap<Self>: Future,
    {
        Unwrap {
            inner: self,
        }
    }

    /// Panic on success, yielding the content of an `Err`.
    fn unwrap_err(self) -> UnwrapErr<Self>
    where
        Self: Sized,
        UnwrapErr<Self>: Future,
    {
        UnwrapErr {
            inner: self,
        }
    }

    /// Panic on success, with a message.
    fn expect_err<T>(self, msg: T) -> ExpectErr<Self>
    where
        Self: Sized,
        ExpectErr<Self>: Future,
        T: fmt::Display,
    {
        ExpectErr {
            inner: self,
            msg: msg.to_string(),
        }
    }

    /// Panic on error, with a message.
    fn expect<T>(self, msg: T) -> Expect<Self>
    where
        Self: Sized,
        Expect<Self>: Future,
        T: fmt::Display,
    {
        Expect {
            inner: self,
            msg: msg.to_string(),
        }
    }

    /// Drive `other` by polling `self`.
    ///
    /// `self` must not resolve before `other` does.
    fn drive<'a, T>(&'a mut self, other: T) -> Drive<'a, Self, T>
    where
        Self: Sized + 'a,
        Drive<'a, Self, T>: Future,
    {
        Drive {
            driver: self,
            future: other,
        }
    }

    /// Wrap this future in one that will yield NotReady once before continuing.
    ///
    /// This allows the executor to poll other futures before trying this one
    /// again.
    #[deprecated(since = "0.3.0", note = "Use `futures::pending!` and async/await instead")]
    fn yield_once(self) -> Box<dyn Future<Output = Self::Output>>
    where
        Self: Future + Sized + 'static,
    {
        Box::new(super::util::yield_once().then(move |_| self))
    }
}

impl<T: Future> FutureExt for T {}

// ===== Unwrap ======

/// Panic on error
// FIXME: This adds little value with async/await
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Unwrap<T> {
    inner: T,
}

impl<T> Unwrap<T> {
    // safe: There is no drop impl, and we don't move `inner` from `poll`
    unsafe_pinned!(inner: T);
}


impl<T> Future for Unwrap<T>
where
    T: TryFuture,
    T::Error: fmt::Debug,
{
    type Output = T::Ok;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.inner().try_poll(cx)
            .map(|r| r.unwrap())
    }
}

// ===== UnwrapErr ======

/// Panic on success.
// FIXME: This adds little value with async/await
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct UnwrapErr<T> {
    inner: T,
}

impl<T> UnwrapErr<T> {
    // safe: There is no drop impl, and we don't move `inner` from `poll`
    unsafe_pinned!(inner: T);
}

impl<T, Item, Error> Future for UnwrapErr<T>
where
    T: Future<Output = Result<Item, Error>>,
    Item: fmt::Debug,
{
    type Output = Error;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.inner().poll(cx)
            .map(|r| r.unwrap_err())
    }
}



// ===== Expect ======

/// Panic on error
// FIXME: This adds little value with async/await
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Expect<T> {
    inner: T,
    msg: String,
}

impl<T> Expect<T> {
    // safe: There is no drop impl, and we don't move `inner` from `poll`
    unsafe_pinned!(inner: T);
}

impl<T, Item, Error> Future for Expect<T>
where
    T: Future<Output = Result<Item, Error>>,
    Error: fmt::Debug,
{
    type Output = Item;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.as_mut()
            .inner()
            .poll(cx)
            .map(|r| r.expect(&self.msg))
    }
}

// ===== ExpectErr ======

/// Panic on success
// FIXME: This adds little value with async/await
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct ExpectErr<T> {
    inner: T,
    msg: String,
}

impl<T> ExpectErr<T> {
    // safe: There is no drop impl, and we don't move `inner` from `poll`
    unsafe_pinned!(inner: T);
}

impl<T, Item, Error> Future for ExpectErr<T>
where
    T: Future<Output = Result<Item, Error>>,
    Item: fmt::Debug,
{
    type Output = Error;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.as_mut()
            .inner()
            .poll(cx)
            .map(|r| r.expect_err(&self.msg))
    }
}

// ===== Drive ======

/// Drive a future to completion while also polling the driver
///
/// This is useful for H2 futures that also require the connection to be polled.
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Drive<'a, T, U> {
    driver: &'a mut T,
    future: U,
}

impl<'a, T, U> Drive<'a, T, U> {
    // safe: There is no drop impl, and we don't move `future` from `poll`
    unsafe_pinned!(future: U);
    // safe: Driver is required to be unpin
    unsafe_unpinned!(driver: T);
}

impl<'a, T, U> Future for Drive<'a, T, U>
where
    T: Future + Unpin,
    U: Future,
{
    type Output = U::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut looped = false;

        loop {
            match self.as_mut().future().poll(cx) {
                Poll::Ready(val) => return Poll::Ready(val),
                Poll::Pending => {},
            }

            if self.as_mut().driver().poll_unpin(cx).is_ready() {
                if looped {
                    // Try polling the future one last time
                    panic!("driver resolved before future")
                } else {
                    looped = true;
                    continue;
                }
            }

            return Poll::Pending;
        }
    }
}
