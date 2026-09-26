//! Transports: a reader thread turns a byte stream into complete <...>
//! messages on a channel; the GUI thread owns a cloned write handle.
//!
//! All socket/serial I/O happens on the reader thread or through the write
//! handle; the GUI never blocks on the link.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serialport::SerialPort;

pub enum RxEvent {
    Msg(String),
    Error(String),
}

/// What one read attempt produced. The three-way split matters:
/// Timeout keeps the loop alive, Eof tears it down as a disconnect.
enum ReadResult {
    Data(Vec<u8>),
    Timeout,
    Eof,
}

trait ReadSome: Send {
    fn read_some(&mut self) -> io::Result<ReadResult>;
}

struct TcpReader(TcpStream);

impl ReadSome for TcpReader {
    fn read_some(&mut self) -> io::Result<ReadResult> {
        let mut buf = [0u8; 1024];
        match self.0.read(&mut buf) {
            Ok(0) => Ok(ReadResult::Eof), // a real FIN from the station
            Ok(n) => Ok(ReadResult::Data(buf[..n].to_vec())),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(ReadResult::Timeout)
            }
            Err(e) => Err(e),
        }
    }
}

struct SerialReader(Box<dyn SerialPort>);

impl ReadSome for SerialReader {
    fn read_some(&mut self) -> io::Result<ReadResult> {
        let mut buf = [0u8; 1024];
        match self.0.read(&mut buf) {
            // A serial link has no orderly close, so an empty read is only
            // ever the 0.4 s timeout expiring on an idle line -- NEVER map it
            // to Eof, or the link drops the moment the station goes quiet.
            // The port vanishing surfaces as a real error from read().
            Ok(0) => Ok(ReadResult::Timeout),
            Ok(n) => Ok(ReadResult::Data(buf[..n].to_vec())),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(ReadResult::Timeout)
            }
            Err(e) => Err(e),
        }
    }
}

/// Accumulate bytes, emit every complete `<...>` frame, discard the rest.
/// Matches the Python MSG_RE semantics: the frame body is the text between
/// the innermost '<' and the next '>'.
fn extract_frames(buf: &mut Vec<u8>, tx: &Sender<RxEvent>) {
    while let Some(gt) = buf.iter().position(|&b| b == b'>') {
        if let Some(lt) = buf[..gt].iter().rposition(|&b| b == b'<') {
            let body = String::from_utf8_lossy(&buf[lt + 1..gt]).into_owned();
            let _ = tx.send(RxEvent::Msg(body));
        }
        buf.drain(..=gt);
    }
    if buf.len() > 4096 {
        buf.clear(); // runaway garbage guard
    }
}

fn read_loop(
    mut reader: Box<dyn ReadSome>,
    stop: Arc<AtomicBool>,
    tx: Sender<RxEvent>,
    repaint: impl Fn() + Send + 'static,
) {
    let mut buf: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match reader.read_some() {
            Ok(ReadResult::Timeout) => continue,
            Ok(ReadResult::Eof) => {
                if !stop.load(Ordering::Relaxed) {
                    let _ = tx.send(RxEvent::Error(
                        "connection closed by command station".to_string(),
                    ));
                    repaint();
                }
                return;
            }
            Ok(ReadResult::Data(data)) => {
                buf.extend_from_slice(&data);
                extract_frames(&mut buf, &tx);
                repaint();
            }
            Err(e) => {
                if !stop.load(Ordering::Relaxed) {
                    let _ = tx.send(RxEvent::Error(e.to_string()));
                    repaint();
                }
                return;
            }
        }
    }
}

enum Writer {
    Tcp(TcpStream),
    Serial(Box<dyn SerialPort>),
}

pub struct Link {
    stop: Arc<AtomicBool>,
    writer: Writer,
}

impl Link {
    pub fn tcp(
        host: &str,
        port: u16,
        tx: Sender<RxEvent>,
        repaint: impl Fn() + Send + 'static,
    ) -> io::Result<Link> {
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "host not found"))?;
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        sock.set_read_timeout(Some(Duration::from_millis(400)))?;
        let reader = sock.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || read_loop(Box::new(TcpReader(reader)), stop2, tx, repaint));
        Ok(Link {
            stop,
            writer: Writer::Tcp(sock),
        })
    }

    pub fn serial(
        port: &str,
        baud: u32,
        tx: Sender<RxEvent>,
        repaint: impl Fn() + Send + 'static,
    ) -> Result<Link, String> {
        let ser = serialport::new(port, baud)
            .timeout(Duration::from_millis(400))
            .open()
            .map_err(|e| e.to_string())?;
        // Let the port settle, then drop whatever accumulated before us
        // (the Python app does the same 0.2 s + reset_input_buffer dance).
        std::thread::sleep(Duration::from_millis(200));
        let _ = ser.clear(serialport::ClearBuffer::Input);
        let reader = ser.try_clone().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::spawn(move || read_loop(Box::new(SerialReader(reader)), stop2, tx, repaint));
        Ok(Link {
            stop,
            writer: Writer::Serial(ser),
        })
    }

    /// Put one command on the wire. The trailing newline is not required by
    /// the protocol but is harmless and helps on serial consoles.
    pub fn send(&mut self, cmd: &str) -> io::Result<()> {
        let line = format!("{cmd}\n");
        match &mut self.writer {
            Writer::Tcp(sock) => sock.write_all(line.as_bytes()),
            Writer::Serial(ser) => ser.write_all(line.as_bytes()),
        }
    }

    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Writer::Tcp(sock) = &self.writer {
            let _ = sock.shutdown(Shutdown::Both);
        }
        // The serial reader clone notices the stop flag within one 0.4 s
        // timeout; the port itself closes when the Link drops.
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

/// Names of the serial ports present right now, USB-looking ones first.
pub fn list_serial_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(ports) => ports.into_iter().map(|p| p.port_name).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn frames(input: &[u8]) -> (Vec<String>, Vec<u8>) {
        let (tx, rx) = channel();
        let mut buf = input.to_vec();
        extract_frames(&mut buf, &tx);
        let mut out = Vec::new();
        while let Ok(RxEvent::Msg(m)) = rx.try_recv() {
            out.push(m);
        }
        (out, buf)
    }

    #[test]
    fn frame_extraction() {
        let (msgs, rest) = frames(b"<p1 MAIN>\n<l 3 -1 130 0>junk<c \"CurrentMAIN\" 250");
        assert_eq!(msgs, vec!["p1 MAIN", "l 3 -1 130 0"]);
        // the incomplete trailing frame stays buffered for the next read
        assert_eq!(rest, b"junk<c \"CurrentMAIN\" 250");
    }

    #[test]
    fn frame_extraction_nested_and_garbage() {
        // innermost '<' wins, matching the Python MSG_RE <([^>]*)>
        let (msgs, _) = frames(b"noise<<s>> <iDCC-EX V-5>");
        assert_eq!(msgs, vec!["s", "iDCC-EX V-5"]);
        // a '>' with no '<' before it is discarded silently
        let (msgs, rest) = frames(b"orphan> <t 3>");
        assert_eq!(msgs, vec!["t 3"]);
        assert!(rest.is_empty());
    }

    #[test]
    fn runaway_buffer_guard() {
        let (tx, _rx) = channel();
        let mut buf = vec![b'x'; 5000]; // garbage, no frames
        extract_frames(&mut buf, &tx);
        assert!(buf.is_empty());
    }

    /// End-to-end over a real socket: connect, receive frames split across
    /// TCP segments, send a command, then see the peer's close as an error
    /// event -- never as silence.
    #[test]
    fn tcp_link_round_trip() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.write_all(b"<iDCC-EX V-5.0.7>\n<l 3 -1 ").unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            sock.write_all(b"130 9>\n").unwrap();
            let mut cmd = [0u8; 64];
            let n = sock.read(&mut cmd).unwrap();
            let got = String::from_utf8_lossy(&cmd[..n]).into_owned();
            drop(sock); // orderly close -> the client must report it
            got
        });

        let (tx, rx) = channel();
        let mut link = Link::tcp("127.0.0.1", addr.port(), tx, || {}).unwrap();
        let timeout = Duration::from_secs(5);
        match rx.recv_timeout(timeout).unwrap() {
            RxEvent::Msg(m) => assert_eq!(m, "iDCC-EX V-5.0.7"),
            RxEvent::Error(e) => panic!("unexpected error: {e}"),
        }
        match rx.recv_timeout(timeout).unwrap() {
            RxEvent::Msg(m) => assert_eq!(m, "l 3 -1 130 9"),
            RxEvent::Error(e) => panic!("unexpected error: {e}"),
        }
        link.send("<s>").unwrap();
        assert_eq!(server.join().unwrap(), "<s>\n");
        match rx.recv_timeout(timeout).unwrap() {
            RxEvent::Error(e) => assert!(e.contains("closed"), "got: {e}"),
            RxEvent::Msg(m) => panic!("unexpected message: {m}"),
        }
        link.close();
    }
}
