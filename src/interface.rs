/// High-level WebSocket library interface

use std::net::SocketAddr;
use std::thread;
use std::sync::mpsc;

use mio::{Token, EventLoop, EventSet, PollOpt, Sender, NotifyError};
use mio::tcp::{TcpListener};
use websocket_essentials::{StatusCode};

use server::{WebSocketServer, SERVER_TOKEN};

#[derive(Clone)]
pub enum WebSocketEvent {
    Connect(Token),
    Close(Token, StatusCode),
    Ping(Token, Box<[u8]>),
    Pong(Token, Box<[u8]>),
    TextMessage(Token, String),
    BinaryMessage(Token, Vec<u8>)
}

pub enum WebSocketInternalMessage {
    GetPeers(mpsc::Sender<Vec<Token>>),
    SendMessage(WebSocketEvent),
    Reregister(Token)
}

pub struct WebSocket {
    events: mpsc::Receiver<WebSocketEvent>,
    event_loop_tx: Sender<WebSocketInternalMessage>
}

impl WebSocket {
    pub fn new(address: SocketAddr) -> WebSocket {
        let (tx, rx) = mpsc::channel();

        let mut event_loop = EventLoop::new().unwrap();
        let event_loop_tx = event_loop.channel();

        thread::spawn(move || {
            let server_socket = TcpListener::bind(&address).unwrap();
            let mut server = WebSocketServer::new(server_socket, tx);

            event_loop.register(&server.socket,
                                SERVER_TOKEN,
                                EventSet::readable(),
                                PollOpt::edge()).unwrap();

            event_loop.run(&mut server).unwrap();
        });

        WebSocket {
            event_loop_tx: event_loop_tx,
            events: rx
        }
    }

    pub fn next(&self) -> WebSocketEvent {
        self.events.recv().unwrap()
    }

    pub fn get_connected(&mut self) -> Result<Vec<Token>, mpsc::RecvError> {
        let (tx, rx) = mpsc::channel();
        self.event_loop_tx.send(WebSocketInternalMessage::GetPeers(tx));
        rx.recv()
    }

    pub fn send(&mut self, msg: WebSocketEvent) {
        self.send_internal(WebSocketInternalMessage::SendMessage(msg));
    }

    fn send_internal(&mut self, msg: WebSocketInternalMessage) -> Result<(), NotifyError<WebSocketInternalMessage>> {
        let mut val = msg;
        loop {
            match self.event_loop_tx.send(val) {
                Err(NotifyError::Full(ret)) => {
                    // The notify queue is full, retry after some time.
                    val = ret;
                    thread::sleep_ms(10);
                },
                result @ _ => return result,
            }
        }
    }
}
