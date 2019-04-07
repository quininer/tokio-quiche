use std::io;
use futures::Poll;
use tokio_udp::UdpSocket;


pub trait LossyIo {
    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, io::Error>;
    fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, io::Error>;
}

impl LossyIo for UdpSocket {
    fn poll_read(&mut self, buf: &mut [u8]) -> Poll<usize, io::Error> {
        self.poll_recv(buf)
    }

    fn poll_write(&mut self, buf: &[u8]) -> Poll<usize, io::Error> {
        self.poll_send(buf)
    }
}

pub fn to_io_error(err: quiche::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
