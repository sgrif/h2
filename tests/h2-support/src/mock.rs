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
    codec: CompatCodec
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
    pub async fn read_preface(mut self) -> Result<Self, io::Error> {
        let mut buf = vec![0; PREFACE.len()];
        self.read_exact(&mut buf).await?;
        assert_eq!(buf, PREFACE);
        Ok(self)
    }

    /// Perform the H2 handshake
    pub async fn assert_client_handshake(
        self,
    ) -> (frame::Settings, Self) {
        self.assert_client_handshake_with_settings(frame::Settings::default())
            .await
    }

    /// Perform the H2 handshake
    pub async fn assert_client_handshake_with_settings<T>(
        mut self,
        settings: T,
    ) -> (frame::Settings, Self)
    where
        T: Into<frame::Settings>,
    {
        let settings = settings.into();
        // Send a settings frame
        self.send(settings.into()).unwrap();

        let me = self.read_preface()
            .await
            .unwrap();
        let (frame, mut me) = me
            .into_future()
            .await;
        let frame = frame.unwrap_or_else(|| panic!("unexpected EOF"))
            .unwrap();
        let settings = if let Frame::Settings(settings) = frame {
            // Send the ACK
            let ack = frame::Settings::ack();

            // TODO: Don't unwrap?
            me.send(ack.into()).unwrap();

            settings
        } else {
            panic!("unexpected frame; frame={:?}", frame);
        };
        let (frame, me) = me.into_future()
            .await;

        let f = assert_settings!(frame.unwrap().unwrap());

        // Is ACK
        assert!(f.is_ack());

        (settings, me)
    }


    /// Perform the H2 handshake
    pub async fn assert_server_handshake(
        self,
    ) -> (frame::Settings, Self) {
        self.assert_server_handshake_with_settings(frame::Settings::default())
            .await
    }

    /// Perform the H2 handshake
    pub async fn assert_server_handshake_with_settings<T>(
        mut self,
        settings: T,
    ) -> (frame::Settings, Self)
    where
        T: Into<frame::Settings>,
    {
        self.write_preface();

        let settings = settings.into();
        self.send(settings.into()).unwrap();

        let frame = self.next().await;
        let frame = frame.unwrap_or_else(|| panic!("unexpected EOF"))
            .unwrap();

        let settings = if let Frame::Settings(settings) = frame {
            // Send the ACK
            let ack = frame::Settings::ack();

            // TODO: Don't unwrap?
            self.send(ack.into()).unwrap();

            settings
        } else {
            panic!("unexpected frame; frame={:?}", frame);
        };

        let frame = self.next().await;

        let f = assert_settings!(frame.unwrap().unwrap());

        // Is ACK
        assert!(f.is_ack());

        (settings, self)
    }
}

impl Stream for Handle {
    type Item = Result<Frame, RecvError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let codec = Pin::new(&mut self.codec);
        codec.poll_next(cx)
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

pub trait HandleFutureExt {
    fn recv_settings(self)
        -> RecvFrame<Pin<Box<dyn Future<Output = (Option<Frame>, Handle)> + Send>>>
    where
        Self: Send + Sized + 'static,
        Self: Future<Output = (frame::Settings, Handle)>,
    {
        self.recv_custom_settings(frame::Settings::default())
    }

    fn recv_custom_settings<T>(self, settings: T)
        -> RecvFrame<Pin<Box<dyn Future<Output = (Option<Frame>, Handle)> + Send>>>
    where
        Self: Send + Sized + 'static,
        Self: Future<Output = (frame::Settings, Handle)>,
        T: Into<frame::Settings>,
    {
        let map = self
            .map(|(settings, handle)| (Some(settings.into()), handle))
            .boxed();

        RecvFrame {
            inner: map,
            frame: Some(settings.into().into()),
        }
    }

    fn ignore_settings(self) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        Self: Send + Sized + 'static,
        Self: Future<Output = (frame::Settings, Handle)>,
    {
        self.map(|(_settings, handle)| handle).boxed()
    }

    fn recv_frame<T>(self, frame: T) -> RecvFrame<<Self as IntoRecvFrame>::Future>
    where
        Self: IntoRecvFrame + Sized,
        T: Into<Frame>,
    {
        self.into_recv_frame(Some(frame.into()))
    }

    fn recv_eof(self) -> RecvFrame<<Self as IntoRecvFrame>::Future>
    where
        Self: IntoRecvFrame + Sized,
    {
        self.into_recv_frame(None)
    }

    fn send_frame<T>(self, frame: T) -> SendFrameFut<Self>
    where
        Self: Sized,
        T: Into<SendFrame>,
    {
        SendFrameFut {
            inner: self,
            frame: Some(frame.into()),
        }
    }

    fn send_bytes(self, data: &[u8]) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        Self: Future<Output = Handle> + Send + Sized + 'static,
    {
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

    fn ping_pong(self, payload: [u8; 8]) -> RecvFrame<<SendFrameFut<Self> as IntoRecvFrame>::Future>
    where
        Self: Sized,
        SendFrameFut<Self>: HandleFutureExt + IntoRecvFrame,
    {
        self.send_frame(frames::ping(payload))
            .recv_frame(frames::ping(payload).pong())
    }

    fn idle_ms(self, ms: usize) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        Self: Send + Sized + 'static,
        Self: Future<Output = Handle>,
    {
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

    fn buffer_bytes(self, num: usize) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        Self: Future<Output = Handle> + Send + Sized + 'static,
    {
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

    fn unbounded_bytes(self) -> Pin<Box<dyn Future<Output = Handle> + Send>>
    where
        Self: Future<Output = Handle> + Send + Sized + 'static,
    {
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

    fn then_notify(self, tx: oneshot::Sender<()>) -> Pin<Box<dyn Future<Output = Self::Output> + Send>>
    where
        Self: Future + Send + Sized + 'static,
    {
        self.inspect(move |_| {
            tx.send(()).unwrap();
        }).boxed()
    }

    fn wait_for<F>(self, other: F) -> Pin<Box<dyn Future<Output = Self::Output> + Send>>
    where
        F: Future + Send + 'static,
        F::Output: Send,
        Self: Future + Send + Sized + 'static,
        Self::Output: Send,
    {
        use futures::future::join;
        join(self, other).map(|(left, _right)| left).boxed()
    }

    fn close(self) -> Pin<Box<dyn Future<Output = ()> + Send>>
    where
        Self: Future + Send + Sized + 'static,
    {
        self.map(drop).boxed()
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct RecvFrame<T> {
    inner: T,
    frame: Option<Frame>,
}

impl<T> RecvFrame<T> {
    // safe: There is no drop impl, and we don't move `future` from `poll`
    unsafe_pinned!(inner: T);
}

impl<T> Future for RecvFrame<T>
where
    T: Future<Output = (Option<Frame>, Handle)>,
{
    type Output = Handle;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        use self::Frame::Data;

        let (frame, handle) = ready!(self.as_mut().inner().poll(cx));

        match (frame, &self.frame) {
            (Some(Data(ref a)), &Some(Data(ref b))) => {
                assert_eq!(a.payload().len(), b.payload().len(), "recv_frame data payload len");
                assert_eq!(a, b, "recv_frame");
            }
            (ref a, b) => {
                assert_eq!(a, b, "recv_frame");
            }
        }

        Poll::Ready(handle)
    }
}

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct SendFrameFut<T> {
    inner: T,
    frame: Option<SendFrame>,
}

impl<T> SendFrameFut<T> {
    // safe: There is no drop impl, and we don't move `future` from `poll`
    unsafe_pinned!(inner: T);
    // safe: We never create a pin to this field, and don't create self-references
    unsafe_unpinned!(frame: Option<SendFrame>);
}

impl<T> Future for SendFrameFut<T>
where
    T: Future<Output = Handle>,
{
    type Output = Handle;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut handle = ready!(self.as_mut().inner().poll(cx));
        handle.send(self.frame().take().unwrap()).unwrap();
        Poll::Ready(handle)
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
    T: Future + 'static,
{
}

pub trait IntoRecvFrame {
    type Future: Future;
    fn into_recv_frame(self, frame: Option<Frame>) -> RecvFrame<Self::Future>;
}

impl IntoRecvFrame for Handle {
    type Future = ::futures::stream::StreamFuture<Self>;

    fn into_recv_frame(self, frame: Option<Frame>) -> RecvFrame<Self::Future> {
        RecvFrame {
            inner: self.into_future(),
            frame: frame,
        }
    }
}

impl<T> IntoRecvFrame for T
where
    T: Future<Output = Handle> + Send + 'static,
{
    type Future = Pin<Box<dyn Future<Output = (Option<Frame>, Handle)> + Send>>;

    fn into_recv_frame(self, frame: Option<Frame>) -> RecvFrame<Self::Future> {
        let into_fut = async {
            let mut handle = self.await;
            let frame = handle.try_next().await.unwrap();
            (frame, handle)
        }.boxed();
        RecvFrame {
            inner: into_fut,
            frame: frame,
        }
    }
}

struct GetWaker;

impl Future for GetWaker {
    type Output = Waker;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready(cx.waker().clone())
    }
}
