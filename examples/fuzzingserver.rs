
extern crate mio_websocket;
extern crate env_logger;

use std::net::SocketAddr;

use mio_websocket::interface::*;

fn main() {
    env_logger::init().unwrap();
    let mut ws = WebSocket::new("127.0.0.1:9002".parse::<SocketAddr>().unwrap());

    loop {
        match ws.next() {
            event @ (_, WebSocketEvent::TextMessage(_)) |
            event @ (_, WebSocketEvent::BinaryMessage(_)) => {
                // Echo back the message that we have received.
                ws.send(event);
            },
            _ => {}
        }
    }
}
