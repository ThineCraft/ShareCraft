//! A small ICAP server (RFC 3507) for development and tests: `cargo xtask fake-icap [--port
//! 1344] [--slow] [--fail]`, and started in-process by the server's tests.
//!
//! It answers `OPTIONS` for its one service, and `RESPMOD` by reading the encapsulated HTTP
//! response body (chunked) a piece at a time, never whole, looking for the EICAR test string
//! anywhere in it: found, it answers `200` with `X-Infection-Found` and `X-Virus-ID`; otherwise
//! `204`. It honours a `Preview` it advertised (answering `100 Continue`, or at once when the
//! preview holds the test string or ends the file with `ieof`). Its behaviour can be changed
//! while it runs, to be slow, fail, answer garbage, stall or drop the connection part way
//! through a body, dribble its answer a byte at a time, or send a head that never ends.
//!
//! Like a strict real scanner, it takes only CRLF line ends: a request line, header or chunk
//! line ended by a bare LF ends the connection without an answer, and is counted.
//!
//! The EICAR string is built here from parts at run time, so no source file or binary holds it
//! whole.
//!
//! It uses the standard library only, so the server's tests can include this file as a module.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// The service the fake server offers, as in `icap://127.0.0.1:1344/scan`.
pub const SERVICE: &str = "scan";
/// The threat name the fake server reports.
pub const THREAT: &str = "Eicar-Test-Signature";
/// How long `--slow` waits before answering each scan.
pub const SLOW: Duration = Duration::from_secs(5);
/// The largest piece of a body read at a time.
const PIECE: usize = 64 * 1024;
/// The longest request head accepted.
const MAX_HEAD: usize = 64 * 1024;

/// The EICAR anti-malware test string, put together from parts.
#[must_use]
pub fn eicar() -> Vec<u8> {
    let parts = [
        "X5O!P%@AP[4\\PZX54(P^)7CC)7}$",
        "EICAR-STANDARD-",
        "ANTIVIRUS-TEST-",
        "FILE!$H+H*",
    ];
    parts.concat().into_bytes()
}

/// How the fake server behaves; changed with [`FakeIcap::set`] while it runs.
#[derive(Clone, Debug, Default)]
pub struct Behaviour {
    /// The `Preview` size advertised by `OPTIONS`; none when `None`.
    pub preview: Option<usize>,
    /// Waits this long before answering a scan.
    pub slow: Option<Duration>,
    /// Answers every scan `500`.
    pub fail: bool,
    /// Answers every scan with bytes that are not ICAP.
    pub garbage: bool,
    /// Stops reading a body once this many of its bytes have arrived, and holds the
    /// connection open, answering nothing, until stopped.
    pub stall_after: Option<u64>,
    /// Closes the connection once this many bytes of a body have arrived.
    pub drop_after: Option<u64>,
    /// Sends each answer to a scan one byte at a time, waiting this long between bytes.
    pub dribble: Option<Duration>,
    /// Answers a scan with a status line and then headers without end.
    pub endless_head: bool,
}

/// What the fake server has seen since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// `OPTIONS` requests.
    pub options: u64,
    /// `RESPMOD` requests.
    pub scans: u64,
    /// Scans that sent a preview.
    pub previews: u64,
    /// Scans answered `100 Continue` after their preview.
    pub continues: u64,
    /// Scans whose preview held the whole file (`ieof`).
    pub ieofs: u64,
    /// Scans answered infected.
    pub infected: u64,
    /// Body bytes received, over all scans.
    pub received: u64,
    /// Requests refused because a line did not end with CRLF, or was otherwise malformed.
    pub malformed: u64,
}

#[derive(Default)]
struct Counters {
    malformed: AtomicU64,
    options: AtomicU64,
    scans: AtomicU64,
    previews: AtomicU64,
    continues: AtomicU64,
    ieofs: AtomicU64,
    infected: AtomicU64,
    received: AtomicU64,
}

/// What every connection shares.
struct Shared {
    behaviour: Mutex<Behaviour>,
    counters: Counters,
    stopping: AtomicBool,
    /// Every connection accepted, to shut them down when stopping.
    connections: Mutex<Vec<TcpStream>>,
}

impl Shared {
    fn behaviour(&self) -> Behaviour {
        self.behaviour
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// A running fake ICAP server; stopped when dropped.
pub struct FakeIcap {
    addr: SocketAddr,
    shared: Arc<Shared>,
    acceptor: Option<JoinHandle<()>>,
}

impl FakeIcap {
    /// Starts a fake server listening on `addr` (port 0 for any free port) with `behaviour`.
    ///
    /// # Errors
    /// The address cannot be bound.
    pub fn start(addr: SocketAddr, behaviour: Behaviour) -> std::io::Result<FakeIcap> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            behaviour: Mutex::new(behaviour),
            counters: Counters::default(),
            stopping: AtomicBool::new(false),
            connections: Mutex::new(Vec::new()),
        });
        let accepting = Arc::clone(&shared);
        let acceptor = thread::spawn(move || accept(&listener, &accepting));
        Ok(FakeIcap {
            addr,
            shared,
            acceptor: Some(acceptor),
        })
    }

    /// The address it listens on.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Its service's address, `icap://<addr>/scan`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("icap://{}/{SERVICE}", self.addr)
    }

    /// Changes how it behaves from the next request on.
    pub fn set(&self, behaviour: Behaviour) {
        *self
            .shared
            .behaviour
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = behaviour;
    }

    /// What it has seen so far.
    #[must_use]
    pub fn seen(&self) -> Seen {
        let c = &self.shared.counters;
        Seen {
            options: c.options.load(Ordering::SeqCst),
            scans: c.scans.load(Ordering::SeqCst),
            previews: c.previews.load(Ordering::SeqCst),
            continues: c.continues.load(Ordering::SeqCst),
            ieofs: c.ieofs.load(Ordering::SeqCst),
            infected: c.infected.load(Ordering::SeqCst),
            received: c.received.load(Ordering::SeqCst),
            malformed: c.malformed.load(Ordering::SeqCst),
        }
    }

    /// Stops listening and shuts every connection down at once, as a server that stops does.
    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        for connection in self
            .shared
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            let _ = connection.shutdown(Shutdown::Both);
        }
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
    }
}

impl Drop for FakeIcap {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accepts connections until stopping, each served on a thread of its own.
fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    while !shared.stopping() {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if let Ok(copy) = stream.try_clone() {
                    shared
                        .connections
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(copy);
                }
                let serving = Arc::clone(shared);
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &serving)
                        && e.kind() == ErrorKind::InvalidData
                    {
                        serving.counters.malformed.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// A request's first line and headers (names lower-cased).
struct Head {
    method: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Reads one CRLF-ended line, without its line end; `None` at the end of the stream.
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader
        .take(u64::try_from(MAX_HEAD).unwrap_or(u64::MAX))
        .read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(None);
    }
    // Every line ends with CRLF; a bare LF, or no line end at all, is refused.
    if !line.ends_with(b"\r\n") {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "a line that does not end with CRLF",
        ));
    }
    line.truncate(line.len() - 2);
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

fn read_head(reader: &mut impl BufRead) -> std::io::Result<Option<Head>> {
    let Some(first) = read_line(reader)? else {
        return Ok(None);
    };
    let method = first.split(' ').next().unwrap_or_default().to_owned();
    let mut headers = Vec::new();
    loop {
        let Some(line) = read_line(reader)? else {
            return Ok(None);
        };
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    Ok(Some(Head { method, headers }))
}

/// Serves the requests of one connection until it closes.
fn serve(stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    while let Some(head) = read_head(&mut reader)? {
        let behaviour = shared.behaviour();
        let keep_going = match head.method.as_str() {
            "OPTIONS" => {
                shared.counters.options.fetch_add(1, Ordering::SeqCst);
                writer.write_all(options_answer(&behaviour).as_bytes())?;
                true
            }
            "RESPMOD" => respmod(&head, &mut reader, &mut writer, shared, &behaviour)?,
            _ => {
                writer.write_all(
                    b"ICAP/1.0 405 Method Not Allowed\r\nEncapsulated: null-body=0\r\n\r\n",
                )?;
                true
            }
        };
        writer.flush()?;
        let closing = head
            .header("connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"));
        if !keep_going || closing || shared.stopping() {
            break;
        }
    }
    Ok(())
}

fn options_answer(behaviour: &Behaviour) -> String {
    let preview = behaviour
        .preview
        .map(|n| format!("Preview: {n}\r\nTransfer-Preview: *\r\n"))
        .unwrap_or_default();
    format!(
        "ICAP/1.0 200 OK\r\nMethods: RESPMOD\r\nService: ShareCraft fake ICAP\r\n\
         ISTag: \"fake-1\"\r\nServer: sharecraft-fake-icap\r\nMax-Connections: 4\r\n\
         Options-TTL: 60\r\nAllow: 204\r\n{preview}Encapsulated: null-body=0\r\n\r\n"
    )
}

/// Looks for the EICAR string in a stream given a piece at a time, keeping only the last few
/// bytes between pieces.
struct Detector {
    needle: Vec<u8>,
    tail: Vec<u8>,
    found: bool,
}

impl Detector {
    fn new() -> Detector {
        Detector {
            needle: eicar(),
            tail: Vec::new(),
            found: false,
        }
    }

    fn feed(&mut self, piece: &[u8]) {
        if self.found {
            return;
        }
        let mut joined = std::mem::take(&mut self.tail);
        joined.extend_from_slice(piece);
        if joined
            .windows(self.needle.len())
            .any(|window| window == self.needle.as_slice())
        {
            self.found = true;
        }
        let keep = self.needle.len().saturating_sub(1).min(joined.len());
        self.tail = joined.split_off(joined.len() - keep);
    }
}

/// What reading a body's chunks came to.
enum Body {
    /// Its zero chunk arrived; with `ieof` when it said so.
    Ended { ieof: bool },
    /// The behaviour stopped reading it: the connection is dropped or held.
    Abandoned,
}

/// Reads chunks up to and including the zero chunk, feeding `detector`, a piece at a time.
fn read_chunks(
    reader: &mut impl BufRead,
    detector: &mut Detector,
    shared: &Shared,
    behaviour: &Behaviour,
    received: &mut u64,
) -> std::io::Result<Body> {
    let mut buffer = vec![0u8; PIECE];
    loop {
        let line = read_line(reader)?.ok_or(ErrorKind::UnexpectedEof)?;
        let (size, extension) = match line.split_once(';') {
            Some((size, extension)) => (size.trim(), extension.trim()),
            None => (line.trim(), ""),
        };
        let mut left = u64::from_str_radix(size, 16).map_err(|_| ErrorKind::InvalidData)?;
        if left == 0 {
            // The (empty) trailer ends with a blank line.
            while read_line(reader)?.is_some_and(|line| !line.is_empty()) {}
            return Ok(Body::Ended {
                ieof: extension == "ieof",
            });
        }
        while left > 0 {
            if behaviour.stall_after.is_some_and(|n| *received >= n) {
                while !shared.stopping() {
                    thread::sleep(Duration::from_millis(20));
                }
                return Ok(Body::Abandoned);
            }
            if behaviour.drop_after.is_some_and(|n| *received >= n) {
                return Ok(Body::Abandoned);
            }
            let want = usize::try_from(left).unwrap_or(PIECE).min(PIECE);
            let piece = buffer.get_mut(..want).ok_or(ErrorKind::InvalidData)?;
            let n = reader.read(piece)?;
            if n == 0 {
                return Err(ErrorKind::UnexpectedEof.into());
            }
            let got = piece.get(..n).ok_or(ErrorKind::InvalidData)?;
            detector.feed(got);
            let n = u64::try_from(n).unwrap_or(0);
            left -= n;
            *received += n;
            shared.counters.received.fetch_add(n, Ordering::SeqCst);
        }
        // The chunk's own line end.
        read_line(reader)?;
    }
}

/// Answers one `RESPMOD`; whether the connection may serve another request.
fn respmod(
    head: &Head,
    reader: &mut impl BufRead,
    writer: &mut TcpStream,
    shared: &Shared,
    behaviour: &Behaviour,
) -> std::io::Result<bool> {
    shared.counters.scans.fetch_add(1, Ordering::SeqCst);
    let encapsulated = head.header("encapsulated").unwrap_or_default().to_owned();
    // The encapsulated HTTP headers come before the body, `res-body` bytes of them.
    let mut headers_length = None;
    let mut has_body = false;
    for part in encapsulated.split(',') {
        if let Some((name, offset)) = part.trim().split_once('=') {
            if name == "res-body" || name == "req-body" {
                has_body = true;
                headers_length = offset.trim().parse::<u64>().ok();
            } else if name == "null-body" {
                headers_length = offset.trim().parse::<u64>().ok();
            }
        }
    }
    let headers_length = headers_length.ok_or(ErrorKind::InvalidData)?;
    std::io::copy(&mut reader.take(headers_length), &mut std::io::sink())?;

    let mut detector = Detector::new();
    let mut received = 0u64;
    if has_body {
        let previewing = head.header("preview").is_some();
        if previewing {
            shared.counters.previews.fetch_add(1, Ordering::SeqCst);
        }
        let body = read_chunks(reader, &mut detector, shared, behaviour, &mut received)?;
        match body {
            Body::Abandoned => {
                let _ = writer.shutdown(Shutdown::Both);
                return Ok(false);
            }
            Body::Ended { ieof } if previewing && ieof => {
                shared.counters.ieofs.fetch_add(1, Ordering::SeqCst);
            }
            Body::Ended { .. } if previewing && !detector.found => {
                writer.write_all(b"ICAP/1.0 100 Continue\r\n\r\n")?;
                writer.flush()?;
                shared.counters.continues.fetch_add(1, Ordering::SeqCst);
                if let Body::Abandoned =
                    read_chunks(reader, &mut detector, shared, behaviour, &mut received)?
                {
                    let _ = writer.shutdown(Shutdown::Both);
                    return Ok(false);
                }
            }
            Body::Ended { .. } => {}
        }
    }
    if let Some(wait) = behaviour.slow {
        let mut waited = Duration::ZERO;
        while waited < wait {
            if shared.stopping() {
                return Ok(false);
            }
            let step = Duration::from_millis(20).min(wait - waited);
            thread::sleep(step);
            waited += step;
        }
    }
    if behaviour.endless_head {
        writer.write_all(b"ICAP/1.0 204 No Content\r\n")?;
        while !shared.stopping() {
            writer.write_all(b"X-Padding: the head of this answer never ends\r\n")?;
        }
        return Ok(false);
    }
    if let Some(gap) = behaviour.dribble {
        let answer: &[u8] =
            b"ICAP/1.0 204 No Content\r\nISTag: \"fake-1\"\r\nEncapsulated: null-body=0\r\n\r\n";
        for byte in answer {
            if shared.stopping() {
                return Ok(false);
            }
            writer.write_all(std::slice::from_ref(byte))?;
            writer.flush()?;
            thread::sleep(gap);
        }
        return Ok(true);
    }
    if behaviour.garbage {
        writer.write_all(b"this is not an ICAP answer\r\n\r\n")?;
        let _ = writer.shutdown(Shutdown::Both);
        return Ok(false);
    }
    if behaviour.fail {
        writer.write_all(
            b"ICAP/1.0 500 Server Error\r\nISTag: \"fake-1\"\r\nEncapsulated: null-body=0\r\n\r\n",
        )?;
        return Ok(true);
    }
    if detector.found {
        shared.counters.infected.fetch_add(1, Ordering::SeqCst);
        let http = "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
        let answer = format!(
            "ICAP/1.0 200 OK\r\nISTag: \"fake-1\"\r\n\
             X-Infection-Found: Type=0; Resolution=2; Threat={THREAT};\r\n\
             X-Virus-ID: {THREAT}\r\nEncapsulated: res-hdr=0, null-body={}\r\n\r\n{http}",
            http.len()
        );
        writer.write_all(answer.as_bytes())?;
    } else {
        writer.write_all(
            b"ICAP/1.0 204 No Content\r\nISTag: \"fake-1\"\r\nEncapsulated: null-body=0\r\n\r\n",
        )?;
    }
    Ok(true)
}

/// `cargo xtask fake-icap [--port 1344] [--slow] [--fail]`: runs until the process ends.
///
/// # Errors
/// An argument it does not know, or a port it cannot listen on.
pub fn run(args: &[String]) -> Result<(), String> {
    let mut port: u16 = 1344;
    let mut behaviour = Behaviour::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|p| p.parse().ok())
                    .ok_or("--port needs a port number")?;
            }
            "--slow" => behaviour.slow = Some(SLOW),
            "--fail" => behaviour.fail = true,
            other => {
                return Err(format!(
                    "unknown argument {other}; usage: cargo xtask fake-icap [--port 1344] \
                     [--slow] [--fail]"
                ));
            }
        }
    }
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let server =
        FakeIcap::start(addr, behaviour).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    println!("fake ICAP server listening on {}", server.url());
    println!("content holding the EICAR test string is infected; anything else is clean");
    loop {
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_finds_the_string_across_pieces() {
        let needle = eicar();
        let mut stream = b"prefix bytes ".to_vec();
        stream.extend_from_slice(&needle);
        stream.extend_from_slice(b" suffix");
        for size in [1, 3, 7, 64] {
            let mut detector = Detector::new();
            for piece in stream.chunks(size) {
                detector.feed(piece);
            }
            assert!(detector.found, "pieces of {size}");
        }
        let mut detector = Detector::new();
        detector.feed(b"nothing to see here");
        assert!(!detector.found);
    }
}
