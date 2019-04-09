use std::{ io, mem };
use std::time::Instant;
use std::sync::{ Arc, Mutex };
use std::collections::HashMap;
use rand::Rng;
use smallvec::SmallVec;
use futures::{ try_ready, Future, Stream, Sink, Poll, Async, StartSend, AsyncSink };
use tokio_timer::Delay;
use tokio_udp::UdpSocket;
use tokio_sync::{ mpsc, oneshot };


const MAX_DATAGRAM_SIZE: usize = 1350;
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

pub struct QuicConnector {
    config: Arc<Mutex<quiche::Config>>
}

pub struct Connecting {
    is_server: bool,
    inner: MidHandshake
}

enum MidHandshake {
    Handshaking(Inner),
    End
}

pub struct Connection {
    anchor: Arc<Anchor>,
    open_send: mpsc::UnboundedSender<oneshot::Sender<InnerStream>>,
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

pub struct Driver {
    is_server: bool,
    inner: Inner,
    max_id: u64,
    close_recv: Option<oneshot::Receiver<()>>,
    close_queue: Vec<u64>,
    incoming_send: mpsc::UnboundedSender<InnerStream>,
    event_send: mpsc::UnboundedSender<(u64, Message)>,
    event_recv: mpsc::UnboundedReceiver<(u64, Message)>,
    open_recv: mpsc::UnboundedReceiver<oneshot::Sender<InnerStream>>,
    stream_map: HashMap<u64, mpsc::UnboundedSender<quiche::Result<Message>>>,
    stream_buf: Vec<u8>
}

pub struct Opening {
    anchor: Option<Arc<Anchor>>,
    rx: oneshot::Receiver<InnerStream>
}

pub struct QuicStream {
    _anchor: Arc<Anchor>,
    inner: InnerStream,
}

struct InnerStream {
    id: u64,
    event_send: mpsc::UnboundedSender<(u64, Message)>,
    rx: mpsc::UnboundedReceiver<quiche::Result<Message>>
}

#[derive(Debug)]
enum Message {
    Bytes(Vec<u8>),
    End(Vec<u8>),
    Close,
}

struct Inner {
    io: UdpSocket,
    #[allow(dead_code)] scid: Vec<u8>,
    connect: Box<quiche::Connection>,
    timer: Option<Delay>,
    send_buf: Vec<u8>,
    send_pos: usize,
    send_end: usize,
    send_flush: bool,
    recv_buf: Vec<u8>,
}

impl From<quiche::Config> for QuicConnector {
    fn from(config: quiche::Config) -> QuicConnector {
        QuicConnector { config: Arc::new(Mutex::new(config)) }
    }
}

impl QuicConnector {
    pub fn connect(&self, server_name: &str, io: UdpSocket) -> io::Result<Connecting> {
        let mut scid = vec![0; 16];
        rand::thread_rng().fill(&mut *scid);
        quiche::connect(Some(server_name), &scid, &mut self.config.lock().unwrap())
            .map(move |connect| Connecting {
                is_server: false,
                inner: MidHandshake::Handshaking(Inner {
                    io, scid, connect,
                    timer: None,
                    send_buf: vec![0; MAX_DATAGRAM_SIZE],
                    send_pos: 0,
                    send_end: 0,
                    send_flush: false,
                    recv_buf: vec![0; STREAM_BUFFER_SIZE]
                })
            })
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }

    pub fn accept(&self, io: UdpSocket) -> io::Result<Connecting> {
        let mut scid = vec![0; 16];
        rand::thread_rng().fill(&mut *scid);
        quiche::accept(&scid, None, &mut self.config.lock().unwrap())
            .map(move |connect| Connecting {
                is_server: false,
                inner: MidHandshake::Handshaking(Inner {
                    io, scid, connect,
                    timer: None,
                    send_buf: vec![0; MAX_DATAGRAM_SIZE],
                    send_pos: 0,
                    send_end: 0,
                    send_flush: false,
                    recv_buf: vec![0; STREAM_BUFFER_SIZE]
                })
            })
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    }
}

impl Inner {
    fn poll_io_complete(&mut self) -> Poll<Option<()>, io::Error> {
        if let Some(timer) = self.timer.as_mut() {
            if let Ok(Async::Ready(())) = timer.poll() {
                self.connect.on_timeout();
            }
        }

        if let Some(timeout) = self.connect.timeout() {
            if let Some(timer) = self.timer.as_mut() {
                timer.reset(Instant::now() + timeout);
            } else {
                self.timer = Some(Delay::new(Instant::now() + timeout));
            }

            let _ = self.timer.poll();
        } else {
            self.timer = None;
        }

        let recv_result = self.poll_recv()?;
        let send_result = self.poll_send()?;

        match (self.connect.is_closed(), recv_result, send_result) {
            (true, ..) => Ok(Async::Ready(None)),
            (false, Async::NotReady, Async::NotReady) => Ok(Async::NotReady),
            (..) => Ok(Async::Ready(Some(())))
        }
    }

    fn poll_send(&mut self) -> Poll<(), io::Error> {
        if self.send_flush {
            while self.send_pos != self.send_end {
                let n = try_ready!(self.io.poll_send(&mut self.send_buf[self.send_pos..]));
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
            Err(quiche::Error::Done) if self.send_pos != self.send_end => (),
            Err(quiche::Error::Done) => return Ok(Async::NotReady),
            Err(quiche::Error::BufferTooShort) => {
                self.send_flush = true;
                return Ok(Async::Ready(()));
            },
            Err(err) => {
                self.connect.close(false, err.to_wire(), b"fail")
                    .map_err(to_io_error)?;
                return Ok(Async::NotReady);
            }
        }

        let n = try_ready!(self.io.poll_send(&mut self.send_buf[self.send_pos..self.send_end]));
        self.send_pos += n;

        Ok(Async::Ready(()))
    }

    fn poll_recv(&mut self) -> Poll<(), io::Error> {
        let n = try_ready!(self.io.poll_recv(&mut self.recv_buf));

        match self.connect.recv(&mut self.recv_buf[..n]) {
            Ok(_) => Ok(Async::Ready(())),
            Err(quiche::Error::Done) => Ok(Async::Ready(())),
            Err(err) => {
                self.connect.close(false, err.to_wire(), b"fail")
                    .map_err(to_io_error)?;
                Ok(Async::NotReady)
            }
        }
    }
}

impl Future for Connecting {
    type Item = (Driver, Connection, Incoming);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let MidHandshake::Handshaking(inner) = &mut self.inner {
            while !inner.connect.is_established() {
                if try_ready!(inner.poll_io_complete()).is_none() && !inner.connect.is_established() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
            }
        }

        match mem::replace(&mut self.inner, MidHandshake::End) {
            MidHandshake::Handshaking(inner) => {
                let (anchor, close_recv) = oneshot::channel();
                let anchor = Arc::new(Anchor(Some(anchor)));
                let (incoming_send, incoming_recv) = mpsc::unbounded_channel();
                let (event_send, event_recv) = mpsc::unbounded_channel();
                let (open_send, open_recv) = mpsc::unbounded_channel();

                let connection = Connection {
                    anchor: Arc::clone(&anchor),
                    open_send
                };

                let incoming = Incoming { anchor, rx: incoming_recv };

                let driver = Driver {
                    is_server: self.is_server,
                    inner, incoming_send,
                    event_send, event_recv, open_recv,
                    max_id: 0,
                    close_recv: Some(close_recv),
                    close_queue: Vec::new(),
                    stream_map: HashMap::new(),
                    stream_buf: vec![0; STREAM_BUFFER_SIZE],
                };

                Ok(Async::Ready((driver, connection, incoming)))
            },
            MidHandshake::End => panic!()
        }
    }
}

impl Future for Driver {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            while let Ok(Async::Ready(Some((id, msg)))) = self.event_recv.poll() {
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
                    if let Some(tx) = self.stream_map.get_mut(&id) {
                        let _ = tx.try_send(Err(err));
                    }
                }
            }

            let readable = self.inner.connect
                .readable()
                .collect::<SmallVec<[_; 2]>>();
            for id in readable {
                if self.inner.connect.stream_finished(id) {
                    continue
                }

                let mut incoming_send = self.incoming_send.clone();
                let event_send = self.event_send.clone();

                let tx = self.stream_map.entry(id)
                    .or_insert_with(move || {
                        let (tx, rx) = mpsc::unbounded_channel();
                        let _ = incoming_send.try_send(InnerStream { id, event_send, rx });
                        tx
                    });

                match self.inner.connect.stream_recv(id, &mut self.stream_buf) {
                    Ok((n, fin)) => {
                        let _ = tx.try_send(Ok(if fin {
                            Message::End(self.stream_buf[..n].to_vec())
                        } else {
                            Message::Bytes(self.stream_buf[..n].to_vec())
                        }));
                    },
                    Err(err) => {
                        let _ = tx.try_send(Err(err));
                    }
                }
            }

            while let Ok(Async::Ready(Some(sender))) = self.open_recv.poll() {
                // always bidi stream
                let id = (self.max_id << 2) + if self.is_server { 1 } else { 0 };
                let event_send = self.event_send.clone();
                let (tx, rx) = mpsc::unbounded_channel();
                if sender.send(InnerStream { id, event_send, rx }).is_ok() {
                    self.stream_map.insert(id, tx);
                    self.max_id += 1;
                }
            }

            if let Some(mut close_recv) = self.close_recv.take() {
                if let Ok(Async::Ready(())) = close_recv.poll() {
                    self.inner.connect.close(true, 0x0, b"closing").map_err(to_io_error)?;
                } else {
                    self.close_recv = Some(close_recv);
                }
            }

            while let Some(id) = self.close_queue.pop() {
                self.stream_map.remove(&id);
            }

            if try_ready!(self.inner.poll_io_complete()).is_none() {
                return Ok(Async::Ready(()));
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
                _anchor: self.anchor.clone()
            }))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            _ => Ok(Async::Ready(None))
        }
    }
}

impl Connection {
    pub fn open(&mut self) -> Opening {
        let (tx, rx) = oneshot::channel();

        if self.open_send.try_send(tx).is_ok() {
            Opening { anchor: Some(self.anchor.clone()), rx }
        } else {
            Opening { anchor: None, rx }
        }
    }
}

impl Future for Opening {
    type Item = QuicStream;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let anchor = match self.anchor.take() {
            Some(anchor) => anchor,
            None => return Err(io::ErrorKind::ConnectionAborted.into())
        };

        match self.rx.poll() {
            Ok(Async::Ready(inner)) => Ok(Async::Ready(QuicStream { _anchor: anchor, inner })),
            Ok(Async::NotReady) => {
                self.anchor = Some(anchor);
                Ok(Async::NotReady)
            },
            Err(_) => Err(io::ErrorKind::ConnectionAborted.into())
        }
    }
}

impl Sink for QuicStream {
    type SinkItem = Vec<u8>;
    type SinkError = io::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.inner.event_send.start_send((self.inner.id, Message::Bytes(item))) {
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Ok(AsyncSink::NotReady((_, Message::Bytes(item)))) => Ok(AsyncSink::NotReady(item)), // TODO unreachable
            Ok(_) => unreachable!(),
            Err(err) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, err))
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        match self.inner.event_send.start_send((self.inner.id, Message::Close)) {
            Ok(AsyncSink::Ready) => Ok(Async::Ready(())),
            Ok(AsyncSink::NotReady(_)) => Ok(Async::NotReady),
            Err(err) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, err))
        }
    }
}

impl Stream for QuicStream {
    type Item = Vec<u8>;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.inner.rx.poll() {
            Ok(Async::Ready(Some(Ok(Message::Bytes(item))))) => Ok(Async::Ready(Some(item))),
            Ok(Async::Ready(Some(Ok(Message::End(item))))) => {
                self.inner.rx.close();
                Ok(Async::Ready(Some(item)))
            },
            Ok(Async::Ready(Some(Err(err)))) => Err(to_io_error(err)),
            Ok(Async::Ready(Some(Ok(Message::Close)))) => {
                self.inner.rx.close();
                Ok(Async::Ready(None))
            },
            Ok(Async::Ready(None)) => Ok(Async::Ready(None)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(io::Error::new(io::ErrorKind::ConnectionAborted, err))
        }
    }
}

fn to_io_error(err: quiche::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
