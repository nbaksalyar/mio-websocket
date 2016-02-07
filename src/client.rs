use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::fmt;
use std::error::Error;
use std::sync::mpsc;
use std::rc::Rc;
use std::cell::RefCell;

use mio::*;
use mio::tcp::*;
use http_muncher::Parser;
use rustc_serialize::base64::{ToBase64, STANDARD};
use sha1::Sha1;

use http::HttpParser;
use websocket_essentials::{Frame, OpCode, BufferedFrameReader};
use interface::{WebSocketEvent, WebSocketInternalMessage};

const WEBSOCKET_KEY: &'static [u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn gen_key(key: &str) -> String {
    let mut m = Sha1::new();
    let mut buf = [0u8; 20];

    m.update(key.as_bytes());
    m.update(WEBSOCKET_KEY);

    m.output(&mut buf);

    return buf.to_base64(STANDARD);
}

enum ClientState {
    AwaitingHandshake(RefCell<Parser<HttpParser>>),
    HandshakeResponse,
    Connected
}

pub struct WebSocketClient {
    pub socket: TcpStream,
    pub interest: EventSet,
    headers: Rc<RefCell<HashMap<String, String>>>,
    state: ClientState,
    outgoing: Vec<Frame>,
    outgoing_bytes: Cursor<Vec<u8>>,
    tx: mpsc::Sender<WebSocketEvent>,
    event_loop_tx: Sender<WebSocketInternalMessage>,
    token: Token,
    frame_reader: BufferedFrameReader,
    want_close_connection: bool
}

impl WebSocketClient {
    pub fn new(socket: TcpStream, token: Token, server_sink: mpsc::Sender<WebSocketEvent>,
               event_loop_sink: Sender<WebSocketInternalMessage>) -> WebSocketClient {
        let headers = Rc::new(RefCell::new(HashMap::new()));

        WebSocketClient {
            socket: socket,
            headers: headers.clone(),
            interest: EventSet::readable(),
            state: ClientState::AwaitingHandshake(RefCell::new(Parser::request(HttpParser {
                current_key: None,
                headers: headers.clone()
            }))),
            outgoing: Vec::new(),
            outgoing_bytes: Cursor::new(Vec::with_capacity(2048)),
            tx: server_sink,
            event_loop_tx: event_loop_sink,
            token: token,
            frame_reader: BufferedFrameReader::new(),
            want_close_connection: false
        }
    }

    pub fn send_message(&mut self, msg: WebSocketEvent) -> Result<(), String> {
        let frame = match msg {
            WebSocketEvent::TextMessage(_, ref data) => Some(Frame::from(&*data.clone())),
            WebSocketEvent::BinaryMessage(_, ref data) => Some(Frame::from(&*data.clone())),
            WebSocketEvent::Close(_) => {
                // FIXME: proper error type
                let close_frame = try!(Frame::close(0, b"Server-initiated close").map_err(|e| e.description().to_owned()));
                Some(close_frame)
            },
            WebSocketEvent::Ping(_, ref payload) => Some(Frame::ping(payload.clone())),
            _ => None
        };

        if frame.is_none() {
            return Err("Wrong message type to send".to_string());
        }

        self.outgoing.push(frame.unwrap());

        if self.interest.is_readable() {
            trace!("{:?}: sending {} frames, switching to write", self.token, self.outgoing.len());

            self.interest.insert(EventSet::writable());
            self.interest.remove(EventSet::readable());

            try!(self.event_loop_tx.send(WebSocketInternalMessage::Reregister(self.token))
                 .map_err(|e| e.description().to_string()));
        }

        Ok(())
    }

    pub fn write(&mut self) {
        match self.state {
            ClientState::HandshakeResponse => self.write_handshake(),
            ClientState::Connected => self.write_frames(),
            _ => {}
        }
    }

    fn write_handshake(&mut self) {
        let headers = self.headers.borrow();
        let response_key = gen_key(&*headers.get("Sec-WebSocket-Key").unwrap());
        let response = fmt::format(format_args!("HTTP/1.1 101 Switching Protocols\r\n\
                                                 Connection: Upgrade\r\n\
                                                 Sec-WebSocket-Accept: {}\r\n\
                                                 Upgrade: websocket\r\n\r\n", response_key));
        self.socket.try_write(response.as_bytes()).unwrap();

        // Change the state
        self.state = ClientState::Connected;

        // Send the connection event
        self.tx.send(WebSocketEvent::Connect(self.token));

        self.interest.remove(EventSet::writable());
        self.interest.insert(EventSet::readable());
    }

    fn serialize_frames(&mut self) -> Vec<u8> {
        // FIXME: calculate capacity
        let mut out_buf = Vec::new();
        {
            for frame in self.outgoing.iter() {
                if let Err(e) = frame.write(&mut out_buf) {
                    println!("error on write: {}", e);
                }
            }
        }
        return out_buf;
    }

    fn write_frames(&mut self) {
        if self.outgoing.len() > 0 {
            let out_buf = self.serialize_frames();
            self.outgoing_bytes = Cursor::new(out_buf);

            // Check if there are close frames
            for frame in self.outgoing.iter() {
                if frame.is_close() {
                    self.want_close_connection = true;
                }
            }

            self.outgoing.clear();
        }

        loop {
            let mut write_buf: [u8; 2048] = [0; 2048];
            let read_bytes = self.outgoing_bytes.read(&mut write_buf).unwrap();

            if read_bytes == 0 {
                // Buffer is exhausted
                trace!("{:?}: wrote all bytes; switching to reading", self.token);
                if self.want_close_connection {
                    trace!("{:?}: closing connection", self.token);
                    self.socket.shutdown(Shutdown::Write);
                }
                self.interest.remove(EventSet::writable());
                self.interest.insert(EventSet::readable());
                break;
            }

            match self.socket.try_write(&write_buf[0..read_bytes]) {
                Ok(Some(write_bytes)) => {
                    trace!("{:?}: wrote {}/{} bytes", self.token, write_bytes, read_bytes);
                    if write_bytes < read_bytes {
                        self.outgoing_bytes.seek(SeekFrom::Current(-(read_bytes as i64 - write_bytes as i64)));
                        break;
                    }
                },
                Ok(None) => {
                    // This write call would block
                    break;
                },
                Err(e) => {
                    println!("Error occured while writing bytes: {}", e);
                    break;
                }
            }
        }
    }

    pub fn read(&mut self) {
        match self.state {
            ClientState::AwaitingHandshake(_) => self.read_handshake(),
            ClientState::Connected => self.read_frame(),
            _ => {}
        }
    }

    fn read_frame(&mut self) {
        loop {
            let mut buf = [0; 2048];
            match self.socket.try_read(&mut buf) {
                Err(e) => {
                    error!("Error while reading socket: {:?}", e);
                    // TODO: return error here
                    return
                },
                Ok(None) =>
                    // Socket buffer has got no more bytes.
                    break,
                Ok(Some(read_bytes)) => {
                    let mut cursor = Cursor::<&[u8]>::new(&buf).take(read_bytes as u64);
                    loop {
                        match self.frame_reader.read(&mut cursor) {
                            Err(e) => {
                                error!("Error while reading frame: {}", e);
                                // TODO: return error here
                                break;
                            },
                            Ok(None) => break,
                            Ok(Some(frame)) => {
                                match frame.get_opcode() {
                                    OpCode::TextFrame => {
                                        let payload = ::std::str::from_utf8(&*frame.payload);
                                        if let Err(e) = payload {
                                            error!("Utf8 decode error: {}", e);
                                            // TODO return error here
                                            break;
                                        }
                                        self.tx.send(WebSocketEvent::TextMessage(self.token, payload.unwrap().to_owned()));
                                    },
                                    OpCode::BinaryFrame => {
                                        self.tx.send(WebSocketEvent::BinaryMessage(self.token, (&*frame.payload).to_owned()));
                                    },
                                    OpCode::Ping => {
                                        self.outgoing.push(Frame::pong(&frame));
                                    },
                                    OpCode::ConnectionClose => {
                                        self.tx.send(WebSocketEvent::Close(self.token));
                                        self.outgoing.push(Frame::close_from(&frame));
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }

        // Write any buffered outgoing frames
        if self.outgoing.len() > 0 {
            trace!("{:?}: sending {} frames, switching to write", self.token, self.outgoing.len());
            self.interest.remove(EventSet::readable());
            self.interest.insert(EventSet::writable());
        }
    }

    fn read_handshake(&mut self) {
        loop {
            let mut buf = [0; 2048];
            match self.socket.try_read(&mut buf) {
                Err(e) => {
                    println!("Error while reading socket: {:?}", e);
                    return
                },
                Ok(None) =>
                    // Socket buffer has got no more bytes.
                    break,
                Ok(Some(_)) => {
                    let is_upgrade = if let ClientState::AwaitingHandshake(ref parser_state) = self.state {
                        let mut parser = parser_state.borrow_mut();
                        parser.parse(&buf);
                        parser.is_upgrade()
                    } else { false };

                    if is_upgrade {
                        // Change the current state
                        self.state = ClientState::HandshakeResponse;

                        // Change current interest to `Writable`
                        self.interest.remove(EventSet::readable());
                        self.interest.insert(EventSet::writable());
                        break;
                    }
                }
            }
        }
    }
}
