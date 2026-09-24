//! `https://` support: TLS 1.3 over the same `TcpStream` plain HTTP
//! uses, via the `embedded-tls` crate.
//!
//! # Security — read before trusting this for anything real
//! The connection is opened with `UnsecureProvider`, i.e. **no
//! certificate-chain validation**. `embedded-tls` itself notes that real
//! certificate verification (`webpki`) only works with `std` today, so
//! this is the only option available to a `no_std` kernel with this
//! crate. That means: the session *is* encrypted (passive eavesdropping
//! on the wire sees ciphertext), but it is *not* authenticated — nothing
//! stops an active attacker who controls the network path (ARP spoofing,
//! a rogue AP, ...) from presenting their own certificate and
//! man-in-the-middling the connection undetected. Treat this like
//! `curl -k`, not like a browser padlock.
//!
//! Randomness for the handshake comes from `rng::Rng`, which prefers the
//! CPU's `RDRAND` and falls back to a non-cryptographic PRNG if it's
//! unavailable — see that module's doc comment. `fetch()` logs which
//! path was used so it's visible per-connection, not just in source.

use crate::net::tcp_stream::TcpStream;
use crate::rng::Rng;
use crate::serial_println;
use alloc::vec;
use alloc::vec::Vec;
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider};

/// TLS records can be up to ~16 KiB; this is the size embedded-tls's own
/// docs recommend for the read side. The write side can be smaller, but
/// we keep it symmetric for simplicity.
const RECORD_BUF_LEN: usize = 16640;

pub async fn get(tcp: TcpStream, host: &str, path: &str) -> Result<Vec<u8>, &'static str> {
    let mut read_buf = vec![0u8; RECORD_BUF_LEN];
    let mut write_buf = vec![0u8; RECORD_BUF_LEN];

    let config = TlsConfig::new().with_server_name(host);
    let rng = Rng::new();
    if rng.used_fallback {
        serial_println!(
            "tls: WARNING — no RDRAND on this CPU, handshake randomness is a non-crypto PRNG"
        );
    }

    let mut tls: TlsConnection<'_, TcpStream, Aes128GcmSha256> =
        TlsConnection::new(tcp, &mut read_buf, &mut write_buf);

    tls.open(TlsContext::new(
        &config,
        UnsecureProvider::new::<Aes128GcmSha256>(rng),
    ))
    .await
    .map_err(|_| "TLS handshake failed")?;
    serial_println!("tls: handshake complete with {host} (certificate NOT verified — see net/tls.rs)");

    let request = alloc::format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: {}\r\n\r\n",
        crate::net::http::MOBILE_USER_AGENT
    );
    tls.write(request.as_bytes())
        .await
        .map_err(|_| "TLS write failed")?;
    // `write()` only appends to an internal record buffer (so several
    // small writes can be coalesced into one TLS record) — it does not
    // itself send anything. Without this, the request sits in that
    // buffer forever and the server never sees it.
    tls.flush().await.map_err(|_| "TLS flush failed")?;

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break, // peer reset / record error — return what we have
        }
    }

    Ok(response)
}
