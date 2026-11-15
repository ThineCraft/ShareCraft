//! A small ICAP client (RFC 3507) for the organisation's malware scanner: `OPTIONS` to learn
//! what the service offers, and `RESPMOD` to have a file scanned.
//!
//! A scan sends the file as the body of an encapsulated HTTP response, chunked, read from
//! `reader` a piece at a time: the file is never held whole in memory. When the service's
//! `OPTIONS` advertises a `Preview`, the first bytes go first and the rest only if the scanner
//! answers `100 Continue`. Each request uses a connection of its own, closed afterwards.
//!
//! `204`, or `200` without an infection header, is clean; `200` or `403` with
//! `X-Infection-Found`, `X-Violations-Found` or `X-Virus-ID` is infected; anything else, a
//! malformed answer, a refused connection or a step that takes longer than the timeout is an
//! error, which the scan worker retries.

use std::fmt::Write as _;
use std::time::Duration;

use records::{ScannerUrl, clean_detail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The most of a file read and sent at a time.
const PIECE: usize = 64 * 1024;
/// The longest answer head (status line and headers) accepted.
const MAX_HEAD: usize = 64 * 1024;
/// The largest preview sent, whatever the service advertises.
const MAX_PREVIEW: u64 = 64 * 1024;

/// What a scanner said of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// The threat's name, as the scanner gave it (cleaned, at most 200 characters).
    Infected(String),
}

/// What the scanning service offers, from its `OPTIONS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// How many bytes it wants first (`Preview`); `None` when it takes no preview.
    pub preview: Option<u64>,
    /// How many connections it takes at once (`Max-Connections`).
    pub max_connections: Option<u32>,
    /// The service's tag (`ISTag`), quotes included.
    pub istag: Option<String>,
    /// What it says it is: its `Server` header, else its `Service` header.
    pub server: Option<String>,
}

/// Why a scan or `OPTIONS` did not get an answer.
#[derive(Debug, thiserror::Error)]
pub enum IcapError {
    /// The scanner could not be reached.
    #[error("cannot reach the scanner: {0}")]
    Connect(std::io::Error),
    /// The connection failed part way.
    #[error("the connection to the scanner failed: {0}")]
    Io(std::io::Error),
    /// The file could not be read.
    #[error("cannot read the file to scan: {0}")]
    Read(std::io::Error),
    /// A step took longer than the timeout.
    #[error("the scanner did not answer in time")]
    TimedOut,
    /// The answer was not ICAP.
    #[error("the scanner's answer is not ICAP: {0}")]
    Malformed(&'static str),
    /// An ICAP status that is neither clean nor infected.
    #[error("the scanner answered {0}")]
    Status(u16),
}

/// An answer's status code and headers (names lower-cased, folded lines joined with `\n`).
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A client of one ICAP service.
#[derive(Clone, Debug)]
pub struct Client {
    url: ScannerUrl,
    timeout: Duration,
}

impl Client {
    /// A client of the service at `url`, giving up on any one step (connecting, sending a
    /// piece, waiting for an answer) after `timeout`.
    #[must_use]
    pub fn new(url: ScannerUrl, timeout: Duration) -> Client {
        Client { url, timeout }
    }

    /// The service's address.
    #[must_use]
    pub fn url(&self) -> &ScannerUrl {
        &self.url
    }

    /// Runs `step`, failing with [`IcapError::TimedOut`] after the timeout.
    async fn step<T>(
        &self,
        step: impl Future<Output = std::io::Result<T>>,
        error: fn(std::io::Error) -> IcapError,
    ) -> Result<T, IcapError> {
        match tokio::time::timeout(self.timeout, step).await {
            Ok(result) => result.map_err(error),
            Err(_) => Err(IcapError::TimedOut),
        }
    }

    async fn connect(&self) -> Result<TcpStream, IcapError> {
        let address = (self.url.connect_host().to_owned(), self.url.port());
        let stream = self
            .step(TcpStream::connect(address), IcapError::Connect)
            .await?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    /// The request line's target and `Host` header value.
    fn target(&self) -> (String, String) {
        let host = format!("{}:{}", self.url.host(), self.url.port());
        (format!("icap://{host}/{}", self.url.service()), host)
    }

    /// Asks the service what it offers.
    ///
    /// # Errors
    /// The scanner cannot be reached, does not answer in time, or answers something other
    /// than `200` ICAP.
    pub async fn options(&self) -> Result<Options, IcapError> {
        let mut stream = self.connect().await?;
        let (target, host) = self.target();
        let request = format!(
            "OPTIONS {target} ICAP/1.0\r\nHost: {host}\r\nUser-Agent: ShareCraft\r\n\
             Encapsulated: null-body=0\r\nConnection: close\r\n\r\n"
        );
        self.step(stream.write_all(request.as_bytes()), IcapError::Io)
            .await?;
        let answer = self.read_answer(&mut stream).await?;
        if answer.status != 200 {
            return Err(IcapError::Status(answer.status));
        }
        Ok(Options {
            preview: answer.header("preview").and_then(|v| v.trim().parse().ok()),
            max_connections: answer
                .header("max-connections")
                .and_then(|v| v.trim().parse().ok()),
            istag: answer.header("istag").map(clean_detail),
            server: answer
                .header("server")
                .or_else(|| answer.header("service"))
                .map(clean_detail),
        })
    }

    /// Asks the service for its options, then scans the `size` bytes `reader` gives, a file
    /// named `name` (see [`Client::scan_with`]).
    ///
    /// # Errors
    /// As [`Client::options`] and [`Client::scan_with`].
    pub async fn scan(
        &self,
        reader: impl AsyncRead + Unpin,
        size: u64,
        name: &str,
    ) -> Result<Verdict, IcapError> {
        let options = self.options().await?;
        self.scan_with(&options, reader, size, name).await
    }

    /// Scans the `size` bytes `reader` gives, a file named `name`, with `RESPMOD`, the service
    /// offering `options`: streamed as chunks, never held whole, its first bytes as a preview
    /// when the service takes one.
    ///
    /// # Errors
    /// The scanner cannot be reached, a step takes longer than the timeout, the file cannot be
    /// read, or the answer is malformed or neither clean nor infected.
    pub async fn scan_with(
        &self,
        options: &Options,
        reader: impl AsyncRead + Unpin,
        size: u64,
        name: &str,
    ) -> Result<Verdict, IcapError> {
        let mut reader = reader.take(size);
        let mut stream = self.connect().await?;
        let (target, host) = self.target();
        let http_request = format!(
            "GET /{} HTTP/1.1\r\nHost: sharecraft\r\n\r\n",
            percent_encode(name)
        );
        let http_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
             Content-Length: {size}\r\n\r\n"
        );
        let preview = options.preview.map(|n| n.min(MAX_PREVIEW));
        let mut head = format!("RESPMOD {target} ICAP/1.0\r\nHost: {host}\r\n");
        head.push_str("User-Agent: ShareCraft\r\nAllow: 204\r\nConnection: close\r\n");
        if let Some(n) = preview {
            // Writing to a string cannot fail.
            let _ = write!(head, "Preview: {n}\r\n");
        }
        let _ = write!(
            head,
            "Encapsulated: req-hdr=0, res-hdr={}, res-body={}\r\n\r\n",
            http_request.len(),
            http_request.len() + http_response.len()
        );
        head.push_str(&http_request);
        head.push_str(&http_response);
        self.step(stream.write_all(head.as_bytes()), IcapError::Io)
            .await?;

        let mut buffer = vec![0u8; PIECE];
        let mut sent: u64 = 0;
        if let Some(n) = preview {
            let first = n.min(size);
            match self
                .send_chunks(&mut stream, &mut reader, &mut buffer, first)
                .await
            {
                Ok(count) => sent = count,
                Err(e) => return self.answer_after_failure(&mut stream, e).await,
            }
            if sent < first {
                return Err(short_file());
            }
            // The whole file fitted in the preview.
            let ended = size <= n;
            let last = if ended {
                "0; ieof

"
            } else {
                "0

"
            };
            if let Err(e) = self
                .step(stream.write_all(last.as_bytes()), IcapError::Io)
                .await
            {
                return self.answer_after_failure(&mut stream, e).await;
            }
            let answer = self.read_answer(&mut stream).await?;
            if answer.status != 100 {
                return verdict(&answer);
            }
            if ended {
                return Err(IcapError::Malformed(
                    "100 Continue after the end of the file",
                ));
            }
        }
        match self
            .send_chunks(&mut stream, &mut reader, &mut buffer, size - sent)
            .await
        {
            Ok(count) => sent += count,
            Err(e) => return self.answer_after_failure(&mut stream, e).await,
        }
        if sent < size {
            return Err(short_file());
        }
        if let Err(e) = self
            .step(
                stream.write_all(
                    b"0

",
                ),
                IcapError::Io,
            )
            .await
        {
            return self.answer_after_failure(&mut stream, e).await;
        }
        let answer = self.read_answer(&mut stream).await?;
        verdict(&answer)
    }

    /// Sends up to `limit` of the file's bytes as chunks, a piece at a time; how many it sent
    /// (fewer when the file ended). The zero chunk is not sent.
    async fn send_chunks(
        &self,
        stream: &mut TcpStream,
        reader: &mut (impl AsyncRead + Unpin),
        buffer: &mut [u8],
        limit: u64,
    ) -> Result<u64, IcapError> {
        let mut sent: u64 = 0;
        while sent < limit {
            let want = usize::try_from(limit - sent)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let piece = buffer
                .get_mut(..want)
                .ok_or(IcapError::Malformed("piece"))?;
            let n = self.step(reader.read(piece), IcapError::Read).await?;
            if n == 0 {
                break;
            }
            let data = piece.get(..n).ok_or(IcapError::Malformed("piece"))?;
            let size_line = format!(
                "{n:x}
"
            );
            self.step(stream.write_all(size_line.as_bytes()), IcapError::Io)
                .await?;
            self.step(stream.write_all(data), IcapError::Io).await?;
            self.step(
                stream.write_all(
                    b"
",
                ),
                IcapError::Io,
            )
            .await?;
            sent += u64::try_from(n).unwrap_or(0);
        }
        Ok(sent)
    }

    /// After sending failed, the scanner may still have answered (and closed): its answer if
    /// there is one, otherwise the failure.
    async fn answer_after_failure(
        &self,
        stream: &mut TcpStream,
        failure: IcapError,
    ) -> Result<Verdict, IcapError> {
        if matches!(failure, IcapError::Read(_)) {
            return Err(failure);
        }
        match self.read_answer(stream).await {
            Ok(answer) if answer.status != 100 => verdict(&answer),
            _ => Err(failure),
        }
    }

    /// Reads an answer's head: the status line and headers.
    async fn read_answer(&self, stream: &mut TcpStream) -> Result<Answer, IcapError> {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        // Byte by byte, so nothing after the head is consumed; heads are short.
        while !head.ends_with(b"\r\n\r\n") && !head.ends_with(b"\n\n") {
            if head.len() >= MAX_HEAD {
                return Err(IcapError::Malformed("the answer's head is too long"));
            }
            let n = self.step(stream.read(&mut byte), IcapError::Io).await?;
            if n == 0 {
                return Err(if head.is_empty() {
                    IcapError::Malformed("the connection closed without an answer")
                } else {
                    IcapError::Malformed("the answer ended part way")
                });
            }
            head.extend_from_slice(&byte);
        }
        parse_head(&head)
    }
}

/// The status and headers of an answer's head.
fn parse_head(head: &[u8]) -> Result<Answer, IcapError> {
    let text = std::str::from_utf8(head).map_err(|_| IcapError::Malformed("not text"))?;
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if parts.next() != Some("ICAP/1.0") {
        return Err(IcapError::Malformed("no ICAP status line"));
    }
    let status = parts
        .next()
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(IcapError::Malformed("no status code"))?;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            // A folded line continues the header before it.
            if let Some((_, value)) = headers.last_mut() {
                value.push('\n');
                value.push_str(line.trim());
            }
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(IcapError::Malformed("a header without a colon"))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Answer { status, headers })
}

/// The file gave fewer bytes than its size.
fn short_file() -> IcapError {
    IcapError::Read(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "the file is shorter than its size",
    ))
}

/// What a final answer says of the file.
fn verdict(answer: &Answer) -> Result<Verdict, IcapError> {
    match answer.status {
        204 => Ok(Verdict::Clean),
        200 | 403 => match threat(answer) {
            Some(threat) => Ok(Verdict::Infected(threat)),
            None if answer.status == 200 => Ok(Verdict::Clean),
            None => Err(IcapError::Status(answer.status)),
        },
        status => Err(IcapError::Status(status)),
    }
}

/// The threat an answer names in `X-Infection-Found` (its `Threat=`), `X-Virus-ID` or
/// `X-Violations-Found` (the threat of its first violation); `None` without any of them.
fn threat(answer: &Answer) -> Option<String> {
    if let Some(found) = answer.header("x-infection-found") {
        let named = found
            .split(';')
            .filter_map(|part| part.trim().strip_prefix("Threat="))
            .next()
            .unwrap_or(found);
        return Some(clean_detail(named));
    }
    if let Some(id) = answer.header("x-virus-id") {
        return Some(clean_detail(id));
    }
    if let Some(violations) = answer.header("x-violations-found") {
        // A count, then four lines per violation: file name, threat, resolution, threat type.
        let threat = violations.split('\n').nth(2).unwrap_or(violations);
        return Some(clean_detail(threat));
    }
    None
}

/// `name` for the encapsulated request's path: every byte but unreserved ones escaped.
fn percent_encode(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            // Writing to a string cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn answer(head: &str) -> Answer {
        parse_head(head.as_bytes()).unwrap()
    }

    #[test]
    fn verdicts() {
        let clean = answer("ICAP/1.0 204 No Content\r\nISTag: x\r\n\r\n");
        assert_eq!(verdict(&clean).unwrap(), Verdict::Clean);
        let plain = answer("ICAP/1.0 200 OK\r\nEncapsulated: null-body=0\r\n\r\n");
        assert_eq!(verdict(&plain).unwrap(), Verdict::Clean);
        let found = answer(
            "ICAP/1.0 200 OK\r\nX-Infection-Found: Type=0; Resolution=2; Threat=Bad.Thing;\r\n\r\n",
        );
        assert_eq!(
            verdict(&found).unwrap(),
            Verdict::Infected("Bad.Thing".to_owned())
        );
        let id = answer("ICAP/1.0 403 Forbidden\r\nX-Virus-ID: Worm.X\r\n\r\n");
        assert_eq!(
            verdict(&id).unwrap(),
            Verdict::Infected("Worm.X".to_owned())
        );
        let violations = answer(
            "ICAP/1.0 200 OK\r\nX-Violations-Found: 1\r\n\tfile.bin\r\n\tTrojan.Y\r\n\t2\r\n\t0\r\n\r\n",
        );
        assert_eq!(
            verdict(&violations).unwrap(),
            Verdict::Infected("Trojan.Y".to_owned())
        );
        let refused = answer("ICAP/1.0 403 Forbidden\r\n\r\n");
        assert!(matches!(verdict(&refused), Err(IcapError::Status(403))));
        let error = answer("ICAP/1.0 500 Server Error\r\n\r\n");
        assert!(matches!(verdict(&error), Err(IcapError::Status(500))));
    }

    #[test]
    fn malformed_heads_are_refused() {
        for head in [
            "HTTP/1.1 200 OK\r\n\r\n",
            "ICAP/1.0 OK\r\n\r\n",
            "ICAP/1.0 2000 OK\r\n\r\n",
            "ICAP/1.0 200 OK\r\nno colon here\r\n\r\n",
            "garbage\r\n\r\n",
        ] {
            assert!(parse_head(head.as_bytes()).is_err(), "{head}");
        }
    }

    #[test]
    fn names_are_escaped() {
        assert_eq!(
            percent_encode("Report \"Q3\"\r\n.xlsx"),
            "Report%20%22Q3%22%0D%0A.xlsx"
        );
    }
}
