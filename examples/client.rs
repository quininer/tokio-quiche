use std::net::SocketAddr;
use tokio::prelude::*;
use tokio::net::UdpSocket;
use tokio::runtime::current_thread;
use tokio_quiche::QuicConnector;


fn main() -> Result<(), Box<std::error::Error + Send + Sync + 'static>> {
    let mut config = quiche::Config::new(0xbabababa)?;
    config.set_idle_timeout(30);
    config.set_application_protos(b"\x05hq-18\x08http/0.9")?;
    config.verify_peer(false);
    config.set_max_packet_size(65535);
    config.set_initial_max_data(10);
    config.set_initial_max_stream_data_bidi_local(10);
    config.set_initial_max_stream_data_bidi_remote(10);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_migration(true);

    let socket = UdpSocket::bind(&SocketAddr::from(([0, 0, 0, 0], 0)))?;
    let addr = SocketAddr::from(([127, 0, 0, 1], 4433));
//    let addr = SocketAddr::from(([45, 77, 96, 66], 4433));
    socket.connect(&addr)?;

    let connect = QuicConnector::from(config)
        .connect("quic.tech", socket)?;

    let fut = connect
        .and_then(|(driver, mut connection, _incoming)| {
            current_thread::spawn(driver.map_err(|err| eprintln!("{:?}", err)));

            let fut = connection.open()
                .and_then(|stream| stream.send(b"GET /Cargo.toml".to_vec()))
                .and_then(|stream| stream.concat2())
                .map(|msg| println!("{}", String::from_utf8_lossy(&msg)));

            fut
        })
        .map_err(|err| panic!("{:?}", err));

    current_thread::run(fut);
    Ok(())
}
