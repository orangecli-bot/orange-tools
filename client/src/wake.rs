//! Optional wake webhook. One POST per batch, no retries (unread re-wake retries are banned): a
//! failed POST is logged; the message itself is already safe in unread.jsonl.
use crate::common::*;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Wake { pub url: String, pub key: String }

/// Reads AIR_WAKE_URL / AIR_WAKE_KEY from the notify.env file. Missing file or empty values = no wake.
pub fn load(path: Option<&Path>) -> R<Option<Wake>> {
    let Some(p) = path else { return Ok(None) };
    if !p.exists() { return Ok(None); }
    let env = parse_env_file(p)?;
    let url = env.get("AIR_WAKE_URL").cloned().unwrap_or_default();
    let key = env.get("AIR_WAKE_KEY").cloned().unwrap_or_default();
    if url.is_empty() || key.is_empty() { return Ok(None); }
    if !url.starts_with("https://") && !url.starts_with("http://127.0.0.1") && !url.starts_with("http://localhost") {
        return Err("AIR_WAKE_URL must be https:// (plain http only to localhost)".into());
    }
    Ok(Some(Wake { url, key }))
}

async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut s: S, head: String, body: &[u8]) -> R<u16> {
    s.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
    s.write_all(body).await.map_err(|e| e.to_string())?;
    s.flush().await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 512];
    let mut n = 0;
    while n < 12 {
        let r = s.read(&mut buf[n..]).await.map_err(|e| e.to_string())?;
        if r == 0 { break; }
        n += r;
    }
    String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| "no HTTP status".into())
}

pub async fn post(url: &str, key: &str, body: &[u8]) -> R<u16> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") { (true, r) } else { (false, url.strip_prefix("http://").ok_or("bad url")?) };
    let (hostport, path) = match rest.find('/') { Some(i) => (&rest[..i], &rest[i..]), None => (rest, "/") };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (hostport, if tls { 443 } else { 80 }),
    };
    let head = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nUser-Agent: air-client/0.1\r\nConnection: close\r\n\r\n", body.len());
    let tcp = tokio::net::TcpStream::connect((host, port)).await.map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).ok();
    if !tls { return exchange(tcp, head, body).await; }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().map_err(|e| e.to_string())?
        .with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let s = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    exchange(s, head, body).await
}
