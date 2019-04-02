mod common;

use std::{ io, mem, cmp };
use std::time::Instant;
use std::sync::{ Arc, Mutex };
use std::collections::{ HashMap, VecDeque, hash_map::Entry };
use bytes::{ Bytes, BytesMut, BufMut };
use smallvec::SmallVec;
use crossbeam_queue::{ ArrayQueue, PushError };
use futures::{ try_ready, Future, Stream, Sink, Poll, Async, StartSend, AsyncSink };
use tokio_timer::Delay;
use tokio_sync::{ mpsc, oneshot };
use common::{ LossyIo, to_io_error };


pub struct QuicConnector {
    config: Arc<Mutex<quiche::Config>>
}

pub struct Connecting<IO> {
    inner: MidHandshake<IO>
}

enum MidHandshake<IO> {
    Handshaking(Inner<IO>),
    End
}

pub struct Connection {
    anchor: Arc<Anchor>,
    trace_id: String,
    alpn: Vec<u8>,
    is_resumed: bool
}

pub struct Incoming {
    anchor: Arc<Anchor>,
    rx: mpsc::UnboundedReceiver<InnerStream>
}

struct Anchor(Option<oneshot::Sender<()>>);

impl Drop for Anchor {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

pub struct Driver<IO> {
    inner: Inner<IO>,
    max_id: u64,
    close_recv: oneshot::Receiver<()>,
    close_queue: Vec<u64>,
    incoming_send: mpsc::UnboundedSender<InnerStream>,
    event_send: mpsc::UnboundedSender<u64>,
    event_recv: mpsc::UnboundedReceiver<u64>,
    stream_map: HashMap<u64, (mpsc::UnboundedSender<quiche::Result<Message>>, Arc<ArrayQueue<Message>>)>,
    send_buf: BytesMut
}

pub struct QuicStream {
    anchor: Arc<Anchor>,
    inner: InnerStream,
    buf: Bytes
}

struct InnerStream {
    id: u64,
    event_send: mpsc::UnboundedSender<u64>,
    queue: Arc<ArrayQueue<Message>>,
    rx: mpsc::UnboundedReceiver<quiche::Result<Message>>
}

impl Drop for InnerStream {
    fn drop(&mut self) {
        let _ = self.queue.push(Message::Close);
    }
}

enum Message {
    Bytes(Bytes),
    End(Bytes),
    Close,
}

struct Inner<IO> {
    io: IO,
    connect: Box<quiche::Connection>,
    timer: Option<Delay>,
    send_buf: Vec<u8>,
    send_pos: usize,
    send_end: usize,
    send_flush: bool,
    recv_buf: Vec<u8>
}

impl<IO: LossyIo> Inner<IO> {
    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        if let Some(timer) = &mut self.timer {
            if let Ok(Async::Ready(())) = timer.poll() {
                self.connect.on_timeout();
            }
        }

        match self.connect.timeout() {
            Some(timeout) => if let Some(timer) = &mut self.timer {
                timer.reset(Instant::now() + timeout);
            } else {
                self.timer = Some(Delay::new(Instant::now() + timeout));
            },
            None => self.timer = None
        }

        self.poll_recv()?;
        self.poll_send()?;

        Ok(Async::NotReady)
    }

    fn poll_send(&mut self) -> Poll<(), io::Error> {
        loop {
            if self.send_flush {
                while self.send_pos != self.send_end {
                    let n = try_ready!(self.io.poll_write(&mut self.send_buf[self.send_pos..]));
                    self.send_pos += n;
                }

                self.send_pos = 0;
                self.send_end = 0;
                self.send_flush = false;
            }

            match self.connect.send(&mut self.send_buf[self.send_end..]) {
                Ok(n) => {
                    self.send_end += n;
                    self.send_flush = self.send_end == self.send_buf.len() - 1;
                },
                Err(quiche::Error::Done) => return Ok(Async::Ready(())),
                Err(quiche::Error::BufferTooShort) => {
                    self.send_flush = true;
                    continue;
                },
                Err(err) => {
                    self.connect.close(false, err.to_wire(), b"fail")
                        .map_err(to_io_error)?;
                    return Ok(Async::NotReady);
                }
            }

            let n = try_ready!(self.io.poll_write(&mut self.send_buf[self.send_pos..self.send_end]));
            self.send_pos += n;
        }
    }

    fn poll_recv(&mut self) -> Poll<(), io::Error> {
        loop {
            let n = try_ready!(self.io.poll_read(&mut self.recv_buf));

            match self.connect.recv(&mut self.recv_buf[..n]) {
                Ok(_) => (),
                Err(quiche::Error::Done) => return Ok(Async::Ready(())),
                Err(quiche::Error::BufferTooShort) => return Ok(Async::NotReady),
                Err(err) => {
                    self.connect.close(false, err.to_wire(), b"fail")
                        .map_err(to_io_error)?;
                    return Ok(Async::NotReady);
                }
            }
        }
    }
}

impl<IO: LossyIo> Future for Connecting<IO> {
    type Item = (Driver<IO>, Connection, Incoming);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let MidHandshake::Handshaking(inner) = &mut self.inner {
            try_ready!(inner.poll_send());

            while !inner.connect.is_established() {
                if inner.connect.is_closed() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }

                try_ready!(inner.poll_complete());
            }
        }

        match mem::replace(&mut self.inner, MidHandshake::End) {
            MidHandshake::Handshaking(inner) => {
                let (anchor, close_recv) = oneshot::channel();
                let anchor = Arc::new(Anchor(Some(anchor)));
                let (incoming_send, incoming_recv) = mpsc::unbounded_channel();
                let (event_send, event_recv) = mpsc::unbounded_channel();

                let connection = Connection {
                    anchor: Arc::clone(&anchor),
                    trace_id: inner.connect.trace_id().to_string(),
                    alpn: inner.connect.application_proto().to_vec(),
                    is_resumed: inner.connect.is_resumed()
                };

                let incoming = Incoming { anchor, rx: incoming_recv };

                let driver = Driver {
                    inner, close_recv, incoming_send,
                    event_send, event_recv,
                    max_id: 0,
                    close_queue: Vec::new(),
                    stream_map: HashMap::new(),
                    send_buf: BytesMut::new(),
                };

                // TODO

                Ok(Async::Ready((driver, connection, incoming)))
            },
            MidHandshake::End => panic!()
        }
    }
}

impl<IO: LossyIo> Future for Driver<IO> {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.inner.poll_complete()?;

            while let Ok(Async::Ready(Some(id))) = self.event_recv.poll() {
                if let Some((tx, rx)) = self.stream_map.get_mut(&id) {
                    while let Ok(msg) = rx.pop() {
                        let result = match msg {
                            Message::Bytes(bytes) => self.inner.connect.stream_send(id, &bytes, false),
                            Message::End(bytes) => self.inner.connect.stream_send(id, &bytes, true),
                            Message::Close => {
                                self.close_queue.push(id);
                                self.inner.connect.stream_send(id, &[], true)
                            }
                        };

                        // TODO always write to end ?
                        if let Err(err) = result {
                            let _ = tx.try_send(Err(err));
                        }
                    }
                }
            }

            let readable = self.inner.connect
                .readable()
                .collect::<SmallVec<[_; 4]>>();
            for id in readable {
                let mut incoming_send = self.incoming_send.clone();
                let event_send = self.event_send.clone();

                let entry = self.stream_map.entry(id);

                if let Entry::Vacant(_) = entry {
                    if id > self.max_id {
                        self.max_id = id;
                    } else {
                        continue
                    }
                }

                let (tx, _) = entry
                    .or_insert_with(move || {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let queue = Arc::new(ArrayQueue::new(8)); // TODO
                        let queue2 = queue.clone();

                        let _ = incoming_send.try_send(InnerStream { id, event_send, queue, rx });

                        (tx, queue2)
                    });

                self.send_buf.reserve(8 * 1024); // TODO

                match self.inner.connect.stream_recv(id, unsafe { self.send_buf.bytes_mut() }) {
                    Ok((n, fin)) => {
                        unsafe { self.send_buf.advance_mut(n) };

                        let _ = tx.try_send(Ok(if fin {
                            Message::End(self.send_buf.take().freeze())
                        } else {
                            Message::Bytes(self.send_buf.take().freeze())
                        }));
                    },
                    Err(err) => {
                        let _ = tx.try_send(Err(err));
                    }
                }
            }

            if let Ok(Async::Ready(())) = self.close_recv.poll() {
                self.inner.connect.close(true, 0x0, b"closing").map_err(to_io_error)?;
            }

            if self.inner.connect.is_closed() {
                return Ok(Async::Ready(()))
            }

            while let Some(id) = self.close_queue.pop() {
                self.stream_map.remove(&id);
            }
        }
    }
}

impl Stream for Incoming {
    type Item = QuicStream;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.rx.poll() {
            Ok(Async::Ready(Some(inner))) => Ok(Async::Ready(Some(QuicStream {
                inner,
                anchor: self.anchor.clone(),
                buf: Bytes::new()
            }))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            _ => Ok(Async::Ready(None))
        }
    }
}

impl io::Read for QuicStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unimplemented!()
    }
}
