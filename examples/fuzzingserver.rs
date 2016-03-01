
extern crate mio_websocket;

use std::net::SocketAddr;

use mio_websocket::interface::*;

fn main() {
    let mut ws = WebSocket::new("127.0.0.1:9002".parse::<SocketAddr>().unwrap());

    loop {
        match ws.next() {
            event @ WebSocketEvent::TextMessage(_, _) |
            event @ WebSocketEvent::BinaryMessage(_, _) => {
                // Echo back the message that we have received.
                ws.send(event);
            },
            _ => {}
        }
    }
}
