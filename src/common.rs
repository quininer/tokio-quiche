use std::io;

pub fn to_io_error(err: quiche::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}
