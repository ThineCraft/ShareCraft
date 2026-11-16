use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{self, ServerConfig};

use crate::Result;

const MAX_HEAD_BYTES: usize = 64 * 1024;
const DEFAULT_LISTEN: &str = "127.0.0.1:8443";
const DEFAULT_TO: &str = "127.0.0.1:8787";
const DEFAULT_NAME: &str = "sharecraft.localhost";

#[derive(Clone)]
struct TlsIdentity {
    certificate: CertificateDer<'static>,
    server_config: Arc<ServerConfig>,
}

pub(crate) fn run(args: &[String]) -> Result<()> {
    let options = Options::parse(args)?;
    ensure_loopback(options.listen)?;
    let identity = tls_identity(&options.name)?;
    let fingerprint = Sha256::digest(identity.certificate.as_ref());
    let fingerprint = fingerprint
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    println!("xtask: certificate SHA-256 {fingerprint}");
    println!(
        "xtask: open https://{}:{}/",
        options.name,
        options.listen.port()
    );

    let runtime = tokio::runtime::Runtime::new()
        .map_err(|error| format!("cannot start async runtime: {error}"))?;
    runtime.block_on(serve(options, identity.server_config))
}

struct Options {
    listen: SocketAddr,
    to: SocketAddr,
    name: String,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut listen = DEFAULT_LISTEN.to_owned();
        let mut to = DEFAULT_TO.to_owned();
        let mut name = DEFAULT_NAME.to_owned();
        let mut index = 0;
        while index < args.len() {
            let flag = args
                .get(index)
                .ok_or_else(|| "cannot read tls-proxy argument".to_owned())?;
            let value = args
                .get(index + 1)
                .ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--listen" => listen.clone_from(value),
                "--to" => to.clone_from(value),
                "--name" => name.clone_from(value),
                _ => return Err(format!("unknown tls-proxy option: {flag}")),
            }
            index += 2;
        }
        let listen = listen
            .parse()
            .map_err(|error| format!("invalid --listen address: {error}"))?;
        let to = to
            .parse()
            .map_err(|error| format!("invalid --to address: {error}"))?;
        if name.is_empty() {
            return Err("--name cannot be empty".to_owned());
        }
        Ok(Self { listen, to, name })
    }
}

fn ensure_loopback(address: SocketAddr) -> Result<()> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "--listen must use a loopback address, not {address}"
        ))
    }
}

fn tls_identity(name: &str) -> Result<TlsIdentity> {
    let certified = rcgen::generate_simple_self_signed(vec![
        name.to_owned(),
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
    ])
    .map_err(|error| format!("cannot generate TLS certificate: {error}"))?;
    let certificate = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));
    let mut server_config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|error| format!("cannot configure TLS versions: {error}"))?
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .map_err(|error| format!("cannot configure TLS certificate: {error}"))?;
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsIdentity {
        certificate,
        server_config: Arc::new(server_config),
    })
}

async fn serve(options: Options, server_config: Arc<ServerConfig>) -> Result<()> {
    let listener = TcpListener::bind(options.listen)
        .await
        .map_err(|error| format!("cannot listen on {}: {error}", options.listen))?;
    println!(
        "xtask: forwarding {} to {} (Ctrl+C to stop)",
        options.listen, options.to
    );
    let acceptor = TlsAcceptor::from(server_config);
    loop {
        let (connection, peer) = listener
            .accept()
            .await
            .map_err(|error| format!("cannot accept connection: {error}"))?;
        let acceptor = acceptor.clone();
        let to = options.to;
        tokio::spawn(async move {
            if let Err(error) = proxy_connection(connection, peer.ip(), to, acceptor).await {
                eprintln!("xtask: TLS proxy connection failed: {error}");
            }
        });
    }
}

#[cfg(test)]
async fn serve_one(
    listener: TcpListener,
    to: SocketAddr,
    server_config: Arc<ServerConfig>,
) -> Result<()> {
    let (connection, peer) = listener
        .accept()
        .await
        .map_err(|error| format!("cannot accept test connection: {error}"))?;
    proxy_connection(connection, peer.ip(), to, TlsAcceptor::from(server_config)).await
}

async fn proxy_connection(
    connection: TcpStream,
    client_ip: IpAddr,
    to: SocketAddr,
    acceptor: TlsAcceptor,
) -> Result<()> {
    let mut client = acceptor
        .accept(connection)
        .await
        .map_err(|error| format!("TLS handshake failed: {error}"))?;
    let head = read_head(&mut client).await?;
    let request_line = std::str::from_utf8(&head)
        .map_err(|_| "request head is not UTF-8".to_owned())?
        .split("\r\n")
        .next()
        .ok_or_else(|| "request line is missing".to_owned())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| "request method is missing".to_owned())?;
    let path = request_parts
        .next()
        .ok_or_else(|| "request path is missing".to_owned())?;
    // Only the first part of the path: link addresses carry their token after it.
    println!("{method} {}", first_segment(path));
    let rewritten = rewrite_head(&head, client_ip)?;
    let mut upstream = TcpStream::connect(to)
        .await
        .map_err(|error| format!("cannot connect to upstream {to}: {error}"))?;
    upstream
        .write_all(&rewritten)
        .await
        .map_err(|error| format!("cannot write request head upstream: {error}"))?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map_err(|error| format!("cannot proxy request: {error}"))?;
    Ok(())
}

async fn read_head(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = reader
            .read(&mut byte)
            .await
            .map_err(|error| format!("cannot read request head: {error}"))?;
        if read == 0 {
            return Err("request ended before its headers were complete".to_owned());
        }
        head.push(byte[0]);
        if head.len() > MAX_HEAD_BYTES {
            return Err("request head exceeds 64 KiB".to_owned());
        }
        if head.ends_with(b"\r\n\r\n") {
            return Ok(head);
        }
    }
}

/// The first segment of a request path (`/l/<token>` is shown as `/l/...`), so no link token
/// reaches the console.
fn first_segment(path: &str) -> String {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let first = parts.next().unwrap_or("");
    if parts.next().is_some() {
        format!("/{first}/...")
    } else {
        format!("/{first}")
    }
}

fn rewrite_head(head: &[u8], client_ip: IpAddr) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8".to_owned())?;
    if !text.ends_with("\r\n\r\n") {
        return Err("request head is incomplete".to_owned());
    }
    let mut lines = text[..text.len() - 4].split("\r\n");
    let request_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| "request line is missing".to_owned())?;
    let mut headers = Vec::new();
    let mut host = None;
    let mut upgrade = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "request contains a malformed header".to_owned())?;
        let name = name.trim();
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.trim());
        }
        if name.eq_ignore_ascii_case("upgrade") {
            upgrade = true;
        }
        headers.push((name, line));
    }
    let mut kept = Vec::new();
    for (name, line) in headers {
        if name.eq_ignore_ascii_case("x-forwarded-for")
            || name.eq_ignore_ascii_case("x-forwarded-proto")
            || name.eq_ignore_ascii_case("x-forwarded-host")
            || (!upgrade && name.eq_ignore_ascii_case("connection"))
        {
            continue;
        }
        kept.push(line);
    }
    let host = host.ok_or_else(|| "request has no Host header".to_owned())?;

    let mut output = String::new();
    output.push_str(request_line);
    output.push_str("\r\n");
    for line in kept {
        output.push_str(line);
        output.push_str("\r\n");
    }
    output.push_str(&format!("X-Forwarded-For: {client_ip}\r\n"));
    output.push_str("X-Forwarded-Proto: https\r\n");
    output.push_str(&format!("X-Forwarded-Host: {host}\r\n"));
    if !upgrade {
        output.push_str("Connection: close\r\n");
    }
    output.push_str("\r\n");
    Ok(output.into_bytes())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::rustls;

    #[test]
    fn forwarded_headers_are_replaced_and_connection_is_closed() {
        let input = b"GET /hello?q=1 HTTP/1.1\r\nHost: sharecraft.localhost:8443\r\n\
            X-Forwarded-For: 203.0.113.7\r\nX-Forwarded-Proto: http\r\n\
            X-Forwarded-Host: attacker.example\r\nConnection: keep-alive\r\n\r\n";

        let rewritten = rewrite_head(input, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.starts_with("GET /hello?q=1 HTTP/1.1\r\n"));
        assert!(rewritten.contains("Host: sharecraft.localhost:8443\r\n"));
        assert!(rewritten.contains("X-Forwarded-For: 127.0.0.1\r\n"));
        assert!(rewritten.contains("X-Forwarded-Proto: https\r\n"));
        assert!(rewritten.contains("X-Forwarded-Host: sharecraft.localhost:8443\r\n"));
        assert!(rewritten.contains("Connection: close\r\n"));
        assert!(!rewritten.contains("203.0.113.7"));
        assert!(!rewritten.contains("attacker.example"));
        assert!(!rewritten.contains("keep-alive"));
    }

    #[test]
    fn websocket_upgrade_keeps_connection_header() {
        let input = b"GET /socket HTTP/1.1\r\nHost: sharecraft.localhost:8443\r\n\
            Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n";

        let rewritten = rewrite_head(input, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let rewritten = String::from_utf8(rewritten).unwrap();

        assert!(rewritten.contains("Connection: Upgrade\r\n"));
        assert!(rewritten.contains("Upgrade: websocket\r\n"));
        assert!(!rewritten.contains("Connection: close\r\n"));
    }

    #[tokio::test]
    async fn oversized_head_is_refused() {
        let input = vec![b'a'; MAX_HEAD_BYTES + 1];
        let error = read_head(&mut input.as_slice()).await.unwrap_err();
        assert!(error.contains("64 KiB"));
    }

    #[tokio::test]
    async fn tls_proxy_adds_forwarding_headers() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let identity = tls_identity("sharecraft.localhost").unwrap();
        let certificate = identity.certificate.clone();
        let server_config = identity.server_config.clone();

        let proxy_task = tokio::spawn(async move {
            serve_one(proxy, upstream_addr, server_config)
                .await
                .unwrap();
        });
        let upstream_task = tokio::spawn(async move {
            let (mut connection, _) = upstream.accept().await.unwrap();
            let head = read_head(&mut connection).await.unwrap();
            connection
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(head).unwrap()
        });

        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
        let stream = TcpStream::connect(proxy_addr).await.unwrap();
        let server_name = "sharecraft.localhost".try_into().unwrap();
        let mut tls = connector.connect(server_name, stream).await.unwrap();
        tls.write_all(
            b"GET /check HTTP/1.1\r\nHost: sharecraft.localhost:8443\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        tls.shutdown().await.unwrap();
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();

        let upstream_head = upstream_task.await.unwrap();
        proxy_task.await.unwrap();
        assert!(upstream_head.contains("X-Forwarded-Proto: https\r\n"));
        assert!(upstream_head.contains("X-Forwarded-For: 127.0.0.1\r\n"));
        assert_eq!(
            response,
            b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn logged_path_hides_tokens() {
        assert_eq!(first_segment("/l/abcdef"), "/l/...");
        assert_eq!(first_segment("/api/l/abcdef/children/x"), "/api/...");
        assert_eq!(first_segment("/signin.html?next=/l/abc"), "/signin.html");
        assert_eq!(first_segment("/"), "/");
    }

    #[test]
    fn non_loopback_listen_address_is_refused() {
        let address = SocketAddr::from(([192, 0, 2, 10], 8443));
        let error = ensure_loopback(address).unwrap_err();
        assert!(error.contains("loopback"));
    }
}
