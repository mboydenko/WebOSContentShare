//! LAN discovery: TV clients broadcast a UDP query and every server on the
//! network answers with its name and HTTP port. The client takes the address
//! from the reply's source, so no IP has to be typed in.

use serde_json::json;
use tokio::net::UdpSocket;
use tracing::{debug, info, warn};

use crate::Shared;

/// Fixed so clients can find the server whatever HTTP port it is configured with.
pub const PORT: u16 = 8201;
pub const QUERY: &[u8] = b"WEBOS-CONTENT-SHARE-DISCOVER";

pub async fn run(state: Shared) {
    let sock = match UdpSocket::bind(("0.0.0.0", PORT)).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Автопоиск отключён: UDP-порт {PORT} недоступен ({e})");
            return;
        }
    };
    info!("Автопоиск: отвечаю на запросы телевизоров по UDP {PORT}");
    let mut buf = [0u8; 512];
    loop {
        // On Windows recv_from also reports ICMP "port unreachable" for earlier sends; just keep going.
        let (n, from) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                debug!("discovery recv: {e}");
                continue;
            }
        };
        if !buf[..n].starts_with(QUERY) {
            continue;
        }
        let cfg = state.cfg();
        let reply = json!({
            "app": "webos-content-share",
            "id": cfg.server_id,
            "name": cfg.server_name,
            "port": state.http_port,
            "version": env!("CARGO_PKG_VERSION"),
        })
        .to_string();
        debug!(%from, "discovery query");
        if let Err(e) = sock.send_to(reply.as_bytes(), from).await {
            debug!("discovery reply to {from}: {e}");
        }
    }
}
