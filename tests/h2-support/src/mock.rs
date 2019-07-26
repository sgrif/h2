use crate::{frames, SendFrame};

use h2::{self, RecvError, SendError};
use h2::frame::{self, Frame};

use futures::channel::oneshot;
use futures::compat::*;
use futures::executor::block_on;
use futures::lock::Mutex;
use futures::prelude::*;
use futures::ready;
use futures::task::*;

use pin_utils::*;

use std::pin::Pin;
use std::sync::Arc;
use std::{cmp, io, usize};

type CompatCodec = Compat01As03Sink<
    crate::Codec<Compat<Pipe>>,
    Frame<::std::io::Cursor<::bytes::Bytes>>,
>;

/// A mock I/O
#[derive(Debug)]
pub struct Mock {
    pipe: Pipe,
}

#[derive(Debug)]
pub struct Handle {
    codec: CompatCodec,
    expected_settings_acks: i32,
}

#[derive(Debug, Clone)]
pub struct Pipe {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    /// Data written by the test case to the h2 lib.
    rx: Vec<u8>,

    /// Notify when data is ready to be received.
    rx_task: Option<Waker>,

    /// Data written by the `h2` library to be read by the test case.
    tx: Vec<u8>,

    /// Notify when data is written. This notifies the test case waiters.
    tx_task: Option<Waker>,

    /// Number of bytes that can be written before `write` returns `NotReady`.
    tx_rem: usize,

    /// Task to notify when write capacity becomes available.
    tx_rem_task: Option<Waker>,

    /// True when the pipe is closed.
    closed: bool,
}

const PREFACE: &'static [u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Create a new mock and handle
pub fn new() -> (Mock, Handle) {
    new_with_write_capacity(usize::MAX)
}

/// Create a new mock and handle allowing up to `cap` bytes to be written.
pub fn new_with_write_capacity(cap: usize) -> (Mock, Handle) {
    let inner = Arc::new(Mutex::new(Inner {
        rx: vec![],
        rx_task: None,
        tx: vec![],
        tx_task: None,
        tx_rem: cap,
        tx_rem_task: None,
        closed: false,
    }));

    let mock = Mock {
        pipe: Pipe {
            inner: inner.clone(),
        },
    };

    let handle = Handle {
        codec: Compat01As03Sink::new(h2::Codec::new(Compat::new(Pipe {
            inner,
        }))),
        expected_settings_acks: 0,
    };

    (mock, handle)
}

// ===== impl Handle =====

impl Handle {
    // FIXME: This should return Pin<&mut Pipe>, but that's impossible
    // while we still have layers of `Compat` in between
    fn pipe(&mut self) -> Pipe {
        self.codec.get_ref().get_ref().get_ref().clone()
    }

    /// Get a pinned mutable reference to inner Codec.
    pub fn codec_mut(&mut self) -> &mut CompatCodec {
        &mut self.codec
    }

    /// Send a frame
    pub fn send(&mut self, item: SendFrame) -> Result<(), SendError> {
        block_on(async {
            // Queue the frame
            self.codec.send(item).await?;

            // Flush the frame
            self.codec.flush().await?;

            Ok(())
        })
    }

    /// Writes the client preface
    pub fn write_preface(&mut self) {
        // Write the connnection preface
        block_on(self.pipe().write_all(PREFACE)).unwrap();
    }

    /// Read the client preface
    pub async fn read_preface(mut self) -> Self {
        let mut buf = vec![0; PREFACE.len()];
        self.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, PREFACE);
        self
    }

    /// Perform the H2 handshake
    pub async fn assert_client_handshake(
        self,
    ) -> Self {
        self.assert_client_handshake_with_settings(frame::Settings::default())
            .await
    }

    /// Sends the given settings, and reads the preface from the client
    pub async fn assert_client_handshake_with_settings<T>(
        mut self,
        settings: T,
    ) -> Self
    where
        T: Into<frame::Settings>,
    {
        let settings = settings.into();
        self.send(frame::Settings::from(settings).into()).unwrap();
        self.expected_settings_acks += 1;
        self.read_preface().await
    }

    /// Perform the H2 handshake
    pub async fn assert_server_handshake(self) -> Self {
        self.assert_server_handshake_with_settings(frame::Settings::default())
            .await
    }

    /// Writes the HTTP/2 preface, and sends the given settings
    pub async fn assert_server_handshake_with_settings<T>(
        mut self,
        settings: T,
    ) -> Self
    where
        T: Into<frame::Settings>,
    {
        self.write_preface();
        self.send(settings.into().into()).unwrap();
        self.expected_settings_acks += 1;
        self
    }

    pub async fn close(mut self) {
        futures::future::poll_fn(move |cx| {
            let _ = self.poll_next_unpin(cx);
            if self.expected_settings_acks == 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }).await
    }

    pub async fn recv_frame<T: Into<Frame>>(&mut self, frame: T) {
        use self::Frame::Data;

        let expected = frame.into();
        let frame = self.next().await
            .unwrap_or_else(|| panic!("Received unexpected EOF"))
            .unwrap();

        if let (Data(a), Data(b)) = (&expected, &frame) {
            assert_eq!(a.payload().len(), b.payload().len(), "recv_frame data payload len");
        }
        assert_eq!(expected, frame, "recv_frame");
    }

    pub async fn recv_eof(&mut self) {
        assert!(self.next().await.is_none(), "Received unexpected frame");
    }

    pub async fn send_frame<T: Into<SendFrame>>(&mut self, item: T) {
        self.codec.send(item.into()).await.unwrap();
        self.codec.flush().await.unwrap();
    }
}

impl Stream for Handle {
    type Item = Result<Frame, RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        match ready!(self.codec.poll_next_unpin(cx)) {
            Some(Ok(Frame::Settings(settings))) => {
                if settings.is_ack() {
                    self.expected_settings_acks -= 1;

                    if self.expected_settings_acks < 0 {
                        panic!("Received unexpected settings ack");
                    }
                    self.poll_next(cx)
                } else {
                    self.send(frame::Settings::ack().into()).unwrap();
                    Poll::Ready(Some(Ok(settings.into())))
                }
            }
            other => Poll::Ready(other),
        }
    }
}

impl AsyncRead for Handle {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let pipe = self.pipe();
        pin_mut!(pipe);
        pipe.poll_read(cx, buf)
    }
}

impl AsyncWrite for Handle {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        let pipe = self.pipe();
        pin_mut!(pipe);
        pipe.poll_write(cx, src)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<io::Result<()>> {
        let pipe = self.pipe();
        pin_mut!(pipe);
        pipe.poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<io::Result<()>> {
        let pipe = self.pipe();
        pin_mut!(pipe);
        pipe.poll_close(cx)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        block_on(self.codec.close()).unwrap();

        let mut me = self.codec.get_ref().get_ref().get_ref().inner.try_lock().unwrap();
        me.closed = true;

        if let Some(task) = me.rx_task.take() {
            task.wake();
        }

        if self.expected_settings_acks > 0 {
            panic!("Never received expected settings ack");
        }
    }
}

// ===== impl Mock =====

impl AsyncRead for Mock {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        assert!(
            buf.len() > 0,
            "attempted read with zero length buffer... wut?"
        );

        let mut me = self.pipe.inner.try_lock().unwrap();

        if me.rx.is_empty() {
            if me.closed {
                return Poll::Ready(Ok(0));
            }

            me.rx_task = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let n = cmp::min(buf.len(), me.rx.len());
        buf[..n].copy_from_slice(&me.rx[..n]);
        me.rx.drain(..n);

        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Mock {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        mut src: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut me = self.pipe.inner.try_lock().unwrap();

        if me.closed {
            return Poll::Ready(Ok(src.len()));
        }

        if me.tx_rem == 0 {
            me.tx_rem_task = Some(cx.waker().clone());
            return Poll::Pending;
        }

        if src.len() > me.tx_rem {
            src = &src[..me.tx_rem];
        }

        me.tx.extend(src);
        me.tx_rem -= src.len();

        if let Some(task) = me.tx_task.take() {
            task.wake();
        }

        Poll::Ready(Ok(src.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut Context,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _: &mut Context,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let mut me = self.pipe.inner.try_lock().unwrap();
        me.closed = true;

        if let Some(task) = me.tx_task.take() {
            task.wake();
        }
    }
}

// ===== impl Pipe =====

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        assert!(
            buf.len() > 0,
            "attempted read with zero length buffer... wut?"
        );

        let mut me = self.inner.try_lock().unwrap();

        if me.tx.is_empty() {
            if me.closed {
                return Poll::Ready(Ok(0));
            }
            me.tx_task = Some(cx.waker().clone());
            return Poll::Pending;
        }

        let n = cmp::min(buf.len(), me.tx.len());
        buf[..n].copy_from_slice(&me.tx[..n]);
        me.tx.drain(..n);

        Poll::Ready(Ok(n))
    }
}

impl AsyncWrite for Pipe {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut me = self.inner.try_lock().unwrap();
        me.rx.extend(src);

        if let Some(task) = me.rx_task.take() {
            task.wake();
        }

        Poll::Ready(Ok(src.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub trait HandleFutureExt: Future<Output = Handle> + Send + Sized + 'static {
    fn recv_settings(self) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        self.recv_custom_settings(frame::Settings::default())
    }

    fn recv_custom_settings<T>(self, settings: T)
        -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        T: Into<frame::Settings>,
    {
        self.recv_frame(settings.into())
    }

    fn recv_frame<T>(self, frame: T) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        T: Into<Frame>,
    {
        let frame = frame.into();
        async {
            let mut handle = self.await;
            handle.recv_frame(frame).await;
            handle
        }.boxed()
    }

    fn recv_eof(self) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        async {
            let mut handle = self.await;
            handle.recv_eof().await;
            handle
        }.boxed()
    }

    fn send_frame<T>(self, frame: T) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        T: Into<SendFrame>,
    {
        let frame = frame.into();
        async {
            let mut handle = self.await;
            handle.send_frame(frame).await;
            handle
        }.boxed()
    }

    fn send_bytes(self, data: &[u8]) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        let data = data.to_owned();
        async move {
            let mut handle = self.await;
            let mut pipe = handle.pipe();
            pipe.write_all(&data)
                .await
                .unwrap_or_else(|e| panic!("write err={:?}", e));
            handle
        }.boxed()
    }

    fn ping_pong(self, payload: [u8; 8]) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        self.send_frame(frames::ping(payload))
            .recv_frame(frames::ping(payload).pong())
    }

    fn idle_ms(self, ms: usize) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        use std::thread;
        use std::time::Duration;

        self.then(move |handle| {
            // This is terrible... but oh well
            let (tx, rx) = oneshot::channel();

            thread::spawn(move || {
                thread::sleep(Duration::from_millis(ms as u64));
                tx.send(()).unwrap();
            });

            Idle {
                handle: Some(handle),
                timeout: rx,
            }
        }).boxed()
    }

    fn buffer_bytes(self, num: usize) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        async move {
            let mut handle = self.await;
            {
                let mut i = handle.codec.get_ref().get_ref().get_ref().inner.lock().await;
                i.tx_rem = num;
            }

            loop {
                let pipe = handle.pipe();
                let mut inner = pipe
                    .inner
                    .lock()
                    .await;

                if inner.tx_rem == 0 {
                    inner.tx_rem = usize::MAX;
                    return handle;
                } else {
                    inner.tx_task = Some(GetWaker.await);
                }
            }
        }.boxed()
    }

    fn unbounded_bytes(self) -> Pin<Box<dyn Future<Output = Handle> + Send>> {
        async {
            let handle = self.await;
            {
                let mut i = handle.codec.get_ref().get_ref().get_ref().inner.lock().await;
                i.tx_rem = usize::MAX;

                if let Some(task) = i.tx_rem_task.take() {
                    task.wake();
                }
            }
            handle
        }.boxed()
    }

    fn then_notify(self, tx: oneshot::Sender<()>) -> Pin<Box<dyn Future<Output = Self::Output> + Send>> {
        self.inspect(move |_| {
            tx.send(()).unwrap();
        }).boxed()
    }

    fn wait_for<F>(self, other: F) -> Pin<Box<dyn Future<Output = Self::Output> + Send>>
    where
        F: Future + Send + 'static,
        F::Output: Send,
        Self::Output: Send,
    {
        use futures::future::join;
        join(self, other).map(|(left, _right)| left).boxed()
    }

    fn close(self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        self.then(Handle::close).boxed()
    }
}

pub struct Idle {
    handle: Option<Handle>,
    timeout: oneshot::Receiver<()>,
}

impl Future for Idle {
    type Output = Handle;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.timeout.poll_unpin(cx).is_ready() {
            return Poll::Ready(self.handle.take().unwrap());
        }

        self.handle.as_mut().unwrap().poll_next_unpin(cx).map(|res| {
            panic!("Idle received unexpected frame on handle; frame={:?}", res);
        })
    }
}

impl<T> HandleFutureExt for T
where
    T: Future<Output = Handle> + Send + 'static,
{
}

struct GetWaker;

impl Future for GetWaker {
    type Output = Waker;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready(cx.waker().clone())
    }
}
