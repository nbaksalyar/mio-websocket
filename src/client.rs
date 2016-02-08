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
use bytes::{Buf, ByteBuf, MutByteBuf};
use byteorder::{ByteOrder, BigEndian};

use http::HttpParser;
use websocket_essentials::{Frame, OpCode, StatusCode, BufferedFrameReader, ParseError};
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
    outgoing_bytes: ByteBuf,
    tx: mpsc::Sender<WebSocketEvent>,
    event_loop_tx: Sender<WebSocketInternalMessage>,
    token: Token,
    frame_reader: BufferedFrameReader,
    close_connection: bool
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
            outgoing_bytes: ByteBuf::none(),
            tx: server_sink,
            event_loop_tx: event_loop_sink,
            token: token,
            frame_reader: BufferedFrameReader::new(),
            close_connection: false
        }
    }

    pub fn send_message(&mut self, msg: WebSocketEvent) -> Result<(), String> {
        let frame = match msg {
            WebSocketEvent::TextMessage(_, ref data) => Some(Frame::from(&*data.clone())),
            WebSocketEvent::BinaryMessage(_, ref data) => Some(Frame::from(&*data.clone())),
            WebSocketEvent::Close(_, status_code) => Some(Frame::close(status_code)),
            WebSocketEvent::Ping(_, ref payload) => Some(Frame::ping(payload.clone())),
            _ => None
        };

        if frame.is_none() {
            return Err("Wrong message type to send".to_string());
        }

        self.outgoing.push(frame.unwrap());

        if self.interest.is_readable() {
            trace!("{:?} sending {} frames, switching to write", self.token, self.outgoing.len());

            self.interest.insert(EventSet::writable());
            self.interest.remove(EventSet::readable());

            try!(self.event_loop_tx.send(WebSocketInternalMessage::Reregister(self.token))
                 .map_err(|e| e.description().to_string()));
        }

        Ok(())
    }

    fn close_with_status(&mut self, status: StatusCode) {
        self.outgoing.push(Frame::close(status));
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
        out_buf
    }

    fn write_frames(&mut self) {
        loop {
            if !self.outgoing_bytes.has_remaining() {
                if self.outgoing.len() > 0 {
                    trace!("{:?} has {} more frames to send in queue", self.token, self.outgoing.len());
                    let out_buf = self.serialize_frames();
                    self.outgoing_bytes = ByteBuf::from_slice(&*out_buf);
                    if !self.close_connection {
                        self.close_connection = self.outgoing.iter().any(|ref frame| frame.is_close());
                    }
                    self.outgoing.clear();
                } else {
                    // Buffer is exhausted and we have no more frames to send out.
                    trace!("{:?} wrote all bytes; switching to reading", self.token);
                    if self.close_connection {
                        trace!("{:?} closing connection", self.token);
                        self.socket.shutdown(Shutdown::Write);
                    }
                    self.interest.remove(EventSet::writable());
                    self.interest.insert(EventSet::readable());
                    break;
                }
            }

            match self.socket.try_write_buf(&mut self.outgoing_bytes) {
                Ok(Some(write_bytes)) => {
                    trace!("{:?} wrote {} bytes, remaining: {}", self.token, write_bytes, self.outgoing_bytes.remaining());
                },
                Ok(None) => {
                    // This write call would block
                    break;
                },
                Err(e) => {
                    // Write error - close this connnection immediately
                    error!("{:?} Error occured while writing bytes: {}", self.token, e);
                    self.interest.remove(EventSet::writable());
                    self.interest.insert(EventSet::hup());
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
            let mut buf = ByteBuf::mut_with_capacity(16384);
            match self.socket.try_read_buf(&mut buf) {
                Err(e) => {
                    error!("{:?} Error while reading socket: {:?}", self.token, e);
                    self.interest.remove(EventSet::readable());
                    self.interest.insert(EventSet::hup());
                    return
                },
                Ok(None) =>
                    // Socket buffer has got no more bytes.
                    break,
                Ok(Some(0)) => {
                    // Remote end has closed connection, we can close it now, too.
                    self.interest.remove(EventSet::readable());
                    self.interest.insert(EventSet::hup());
                    return;
                },
                Ok(Some(read_bytes)) => {
                    trace!("{:?} read {} bytes", self.token, read_bytes);
                    let mut read_buf = buf.flip();
                    let mut frames_cnt = 0;
                    loop {
                        match self.frame_reader.read(&mut read_buf) {
                            Err(err @ ParseError::InvalidOpCode(..)) => {
                                error!("{:?} Invalid OpCode: {}", self.token, err);
                                self.close_with_status(StatusCode::ProtocolError);
                                break;
                            },
                            Err(e) => {
                                error!("{:?} Error while reading frame: {}", self.token, e);
                                self.interest.remove(EventSet::readable());
                                self.interest.insert(EventSet::hup());
                                return;
                            },
                            Ok(None) => break,
                            Ok(Some(frame)) => {
                                frames_cnt += 1;
                                match frame.get_opcode() {
                                    OpCode::TextFrame => {
                                        let payload = ::std::str::from_utf8(&*frame.payload);
                                        if let Err(e) = payload {
                                            error!("{:?} Utf8 decode error: {}", self.token, e);
                                            self.close_with_status(StatusCode::ProtocolError);
                                            break;
                                        }
                                        self.tx.send(WebSocketEvent::TextMessage(self.token, payload.unwrap().to_owned()));
                                    },
                                    OpCode::BinaryFrame => {
                                        self.tx.send(WebSocketEvent::BinaryMessage(self.token, (&*frame.payload).to_owned()));
                                    },
                                    OpCode::Ping => {
                                        if frame.payload.len() > 125 {
                                            error!("{:?} Control frame length is > 125", self.token);
                                            self.close_with_status(StatusCode::ProtocolError);
                                        } else {
                                            self.outgoing.push(Frame::pong(&frame));
                                        }
                                    },
                                    OpCode::ConnectionClose => {
                                        let close_ev = if frame.payload.len() >= 2 {
                                            let status_code = BigEndian::read_u16(&frame.payload[0..2]);
                                            WebSocketEvent::Close(self.token, StatusCode::from(status_code))
                                        } else {
                                            // No status code has been provided
                                            WebSocketEvent::Close(self.token, StatusCode::Custom(0))
                                        };
                                        self.tx.send(close_ev);

                                        if let Ok(response) = Frame::close_from(&frame) {
                                            self.outgoing.push(response);
                                        } else {
                                            self.close_with_status(StatusCode::ProtocolError);
                                        }
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                    trace!("{:?} parsed {} frames", self.token, frames_cnt);
                    buf = read_buf.flip();
                }
            }
        }

        // Write any buffered outgoing frames
        if self.outgoing.len() > 0 {
            trace!("{:?} received {} frames, switching to write", self.token, self.outgoing.len());
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
