//! A fake outside secret provider for development: `cargo xtask fake-secret-provider --port
//! <n> --token-file <path>`.
//!
//! It serves the provider contract from memory on 127.0.0.1 only, so nothing outlives it:
//!
//! ```text
//! PUT    /v1/secrets/{name}   body {"value":"<base64>"}   -> 204
//! GET    /v1/secrets/{name}                                -> 200 {"value":"<base64>"} | 404
//! DELETE /v1/secrets/{name}                                -> 204 | 404
//! Authorization: Bearer <token>
//! ```
//!
//! A request with another token answers 401. When the token file does not exist, a random token
//! is written to it first (and the file is not overwritten when it does exist). Point the
//! server at it with `SHARECRAFT_SECRET_PROVIDER=http://127.0.0.1:<n>` and
//! `SHARECRAFT_SECRET_PROVIDER_TOKEN_FILE=<path>`. Names must be `sharecraft-` and 32
//! lower-case hex digits. Neither the token nor a value is ever printed.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The port used unless `--port` names another.
const DEFAULT_PORT: u16 = 8200;
/// The longest request head or body accepted.
const MAX_REQUEST: usize = 1 << 20;
/// How long a connection may sit idle.
const IDLE: Duration = Duration::from_secs(30);

/// The values held, as the base64 text each was given.
type Store = Arc<Mutex<HashMap<String, String>>>;

/// Runs `cargo xtask fake-secret-provider` with `args` (everything after the command).
pub fn run(args: &[String]) -> Result<(), String> {
    let mut port = DEFAULT_PORT;
    let mut token_file: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" => {
                port = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--port needs a port number")?;
            }
            "--token-file" => {
                token_file = Some(it.next().ok_or("--token-file needs a path")?.clone());
            }
            other => {
                return Err(format!(
                    "unknown argument {other}; usage: cargo xtask fake-secret-provider --port <n> \
                     --token-file <path>"
                ));
            }
        }
    }
    let token_file = token_file.ok_or("--token-file <path> is required")?;
    let token = token_of(Path::new(&token_file))?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    println!("fake secret provider on http://{bound} (token in {token_file})");
    serve(&listener, &token);
    Ok(())
}

/// The token in `path`, made from random bytes first when the file does not exist.
fn token_of(path: &Path) -> Result<String, String> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let mut bytes = [0u8; 24];
            random_bytes(&mut bytes);
            let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            file.write_all(token.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let token = text.trim().to_owned();
            if token.is_empty() {
                return Err(format!("{} holds no token", path.display()));
            }
            Ok(token)
        }
        Err(e) => Err(format!("cannot create {}: {e}", path.display())),
    }
}

/// Fills `bytes` from the standard library's hasher keys, which the operating system's random
/// source seeds for every new `RandomState`. Enough for a development token that lives in a
/// file on this machine.
fn random_bytes(bytes: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    for chunk in bytes.chunks_mut(8) {
        let word = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
            .to_le_bytes();
        for (to, from) in chunk.iter_mut().zip(word) {
            *to = from;
        }
    }
}

fn serve(listener: &TcpListener, token: &str) {
    let store: Store = Arc::default();
    for stream in listener.incoming().flatten() {
        let store = Arc::clone(&store);
        let token = token.to_owned();
        thread::spawn(move || {
            let _ = handle(stream, &store, &token);
        });
    }
}

/// One request: its method and path, whether its bearer token was right, and its body.
struct Request {
    method: String,
    path: String,
    authorised: bool,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream, token: &str) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut length = 0usize;
    let mut authorised = false;
    let mut head = line.len();
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header)?;
        head += n;
        if n == 0 || head > MAX_REQUEST {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("authorization") {
                authorised = value.strip_prefix("Bearer ").is_some_and(|v| v == token);
            }
        }
    }
    if length > MAX_REQUEST {
        return Ok(None);
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(Request {
        method,
        path,
        authorised,
        body,
    }))
}

fn handle(mut stream: TcpStream, store: &Store, token: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IDLE))?;
    let Some(request) = read_request(&stream, token)? else {
        return Ok(());
    };
    let (status, body) = answer(&request, store);
    let text = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Whether `name` is `sharecraft-` and 32 lower-case hex digits.
fn contract_name(name: &str) -> bool {
    name.strip_prefix("sharecraft-").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn answer(request: &Request, store: &Store) -> (u16, String) {
    if !request.authorised {
        return (401, String::new());
    }
    let Some(name) = request.path.strip_prefix("/v1/secrets/") else {
        return (404, String::new());
    };
    if !contract_name(name) {
        return (400, String::new());
    }
    let mut held = store.lock().unwrap_or_else(PoisonError::into_inner);
    match request.method.as_str() {
        "PUT" => {
            let value = serde_json::from_slice::<serde_json::Value>(&request.body)
                .ok()
                .and_then(|v| v.get("value")?.as_str().map(str::to_owned));
            match value {
                Some(value) if STANDARD.decode(&value).is_ok() => {
                    held.insert(name.to_owned(), value);
                    (204, String::new())
                }
                _ => (400, String::new()),
            }
        }
        "GET" => held.get(name).map_or((404, String::new()), |value| {
            (200, format!("{{\"value\":\"{value}\"}}"))
        }),
        "DELETE" => {
            if held.remove(name).is_some() {
                (204, String::new())
            } else {
                (404, String::new())
            }
        }
        _ => (405, String::new()),
    }
}
