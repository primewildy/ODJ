//! UPnP MediaRenderer discovery + control.
//!
//! Pure `std::net` — no tokio, no UPnP crate. The whole feature only
//! needs SSDP (one UDP multicast send + a brief unicast collect), a
//! couple of HTTP GETs for the device descriptions, and a few SOAP
//! POSTs for AVTransport control. All of that is small enough to do
//! with raw sockets and string templating.
//!
//! Naim hardware tested: Mu-so 1st gen (Rygel-based UPnP),
//! UnitiQute / NaimUniti (Naim's own KnOS UPnP stack). Both speak
//! `AVTransport:1` or `:2` cleanly; the SOAP envelopes here use the
//! `:1` namespace which is accepted by both.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One MediaRenderer discovered on the LAN. The `udn` is the stable
/// identifier (e.g. `uuid:d110347e-…`) — that's what settings.toml
/// remembers, since the friendly name + IP can change.
#[derive(Debug, Clone)]
pub struct Renderer {
    pub udn: String,
    pub name: String,
    pub address: IpAddr,
    /// URL the device-description XML was fetched from. Persisted to
    /// settings.toml as a fallback so we can re-probe a pinned
    /// renderer directly when its SSDP advertiser is asleep but the
    /// HTTP server is alive (common Qute / older-NaimUniti post-
    /// standby behaviour). On a fresh SSDP discovery that returns a
    /// new URL for the same UDN, the persisted value gets refreshed.
    pub descriptor_url: String,
    /// Fully-qualified URL for SOAP POSTs to the AVTransport service.
    /// Used by the §3 control layer (SetAVTransportURI + Play / Stop).
    #[allow(dead_code)] // wired up in network-output §3
    pub av_transport_control: String,
    /// When this entry was last seen alive on the LAN.
    last_seen: Instant,
}

impl Renderer {
    /// Drop the entry if we haven't seen it for this long. Naim
    /// devices re-advertise every ~30 s.
    const STALE_AFTER: Duration = Duration::from_secs(90);
}

/// Handle to the background discovery thread. Cheap to clone — the
/// shared list lives behind an `Arc<Mutex<>>`.
#[derive(Clone)]
pub struct DiscoveryHandle {
    inner: Arc<Mutex<HashMap<String, Renderer>>>,
    /// Persisted descriptor URLs to probe directly each sweep, in
    /// case their owner is SSDP-silent right now. Set at construction
    /// from `settings.network_renderer_descriptor_url` — extending to
    /// a list is mechanical when we ever support multiple pinned
    /// renderers.
    seed_urls: Arc<Mutex<Vec<String>>>,
}

impl DiscoveryHandle {
    /// Spawn the discovery loop. Re-scans every 10 s; results land
    /// in the shared map within a few seconds of each sweep. `seeds`
    /// is the cache of every-ever-known descriptor URL — the loop
    /// probes each one directly in addition to SSDP, so a renderer
    /// whose SSDP advertiser is asleep (typical Qute post-standby)
    /// still appears as long as its HTTP server answers.
    pub fn spawn(seeds: Vec<String>) -> Self {
        let inner: Arc<Mutex<HashMap<String, Renderer>>> = Arc::new(Mutex::new(HashMap::new()));
        let seed_urls = Arc::new(Mutex::new(seeds));
        let inner_thread = Arc::clone(&inner);
        let seeds_thread = Arc::clone(&seed_urls);
        std::thread::spawn(move || {
            loop {
                let mut found = scan_once(Duration::from_secs(4));
                // Direct-probe any cached seed URLs — catches
                // renderers whose SSDP advertiser is asleep but
                // whose HTTP server is alive.
                let seeds: Vec<String> = seeds_thread.lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                for url in seeds {
                    // Skip if SSDP already returned this URL — no
                    // need to spend an HTTP roundtrip we'd discard.
                    if found.iter().any(|r| r.descriptor_url == url) { continue; }
                    if let Some(r) = probe_descriptor_url(&url) {
                        found.push(r);
                    }
                }
                if let Ok(mut map) = inner_thread.lock() {
                    let now = Instant::now();
                    for r in found {
                        map.insert(r.udn.clone(), r);
                    }
                    map.retain(|_, r| now.duration_since(r.last_seen) < Renderer::STALE_AFTER);
                }
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        DiscoveryHandle { inner, seed_urls }
    }

    /// Replace the cached seed URLs. Caller passes the full set;
    /// dedup is its responsibility (we don't want to spend an HTTP
    /// roundtrip per duplicate per sweep).
    pub fn set_seed_urls(&self, urls: Vec<String>) {
        if let Ok(mut s) = self.seed_urls.lock() {
            *s = urls;
        }
    }

    /// Snapshot of the current renderers, sorted by friendly name for
    /// stable UI ordering frame-to-frame.
    pub fn renderers(&self) -> Vec<Renderer> {
        let Ok(map) = self.inner.lock() else { return Vec::new(); };
        let mut v: Vec<Renderer> = map.values().cloned().collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        v
    }

    /// Look up a renderer by its UDN (used when settings.toml has a
    /// pinned selection and we need to find it in the live list).
    /// Used by §3 to resolve the persisted selection at startup.
    #[allow(dead_code)] // wired up in network-output §3
    pub fn by_udn(&self, udn: &str) -> Option<Renderer> {
        self.inner.lock().ok()?.get(udn).cloned()
    }
}

/// One synchronous SSDP sweep — sends an M-SEARCH, collects all
/// MediaRenderer responses for up to `budget`, hits each one's device
/// description, returns the parsed renderers. Returns an empty list
/// on any socket / parse error rather than propagating — discovery is
/// best-effort and the UI just shows whatever the last good sweep
/// found.
fn scan_once(budget: Duration) -> Vec<Renderer> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return Vec::new(); };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
    let msearch = concat!(
        "M-SEARCH * HTTP/1.1\r\n",
        "HOST: 239.255.255.250:1900\r\n",
        "MAN: \"ssdp:discover\"\r\n",
        "MX: 3\r\n",
        "ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n",
        "USER-AGENT: ODJ/0.1 UPnP/1.1\r\n",
        "\r\n",
    );
    if sock.send_to(msearch.as_bytes(), "239.255.255.250:1900").is_err() {
        return Vec::new();
    }

    let deadline = Instant::now() + budget;
    let mut locations: HashMap<String, (String, IpAddr)> = HashMap::new(); // usn → (location, ip)
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        let Ok((n, addr)) = sock.recv_from(&mut buf) else { continue; };
        let text = match std::str::from_utf8(&buf[..n]) { Ok(s) => s, Err(_) => continue };
        // SSDP headers look like `LOCATION: http://host:port/path` —
        // the value's own colons mean we can't `split_once(':')` once
        // and use both halves. `splitn(2, ':')` does the right thing.
        let mut usn = String::new();
        let mut location = String::new();
        for line in text.lines() {
            let mut it = line.splitn(2, ':');
            let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
            match k.trim().to_ascii_lowercase().as_str() {
                "location" => location = v.trim().to_string(),
                "usn"      => usn      = v.trim().to_string(),
                _ => {}
            }
        }
        if usn.is_empty() || location.is_empty() { continue; }
        locations.entry(usn).or_insert((location, addr.ip()));
    }

    let mut out = Vec::new();
    for (_usn, (location, ip)) in locations {
        if let Some(r) = fetch_renderer(&location, ip) {
            out.push(r);
        }
    }
    out
}

/// HTTP GET the device-description XML at `location`, parse out the
/// fields we care about, and return a `Renderer`. None on any error.
fn fetch_renderer(location: &str, address: IpAddr) -> Option<Renderer> {
    let (host, port, path) = split_url(location)?;
    let body = http_get(&host, port, &path, Duration::from_secs(2))?;
    let udn = extract_tag(&body, "UDN")?;
    let name = extract_tag(&body, "friendlyName").unwrap_or_else(|| udn.clone());
    let av_path = extract_avtransport_control_url(&body)?;
    let av_url = absolutise(&host, port, &av_path);
    Some(Renderer {
        udn,
        name,
        address,
        descriptor_url: location.to_string(),
        av_transport_control: av_url,
        last_seen: Instant::now(),
    })
}

/// Direct-probe a previously-known descriptor URL. Same parse as
/// `fetch_renderer`, but we resolve the device's current IP from the
/// URL host (rather than from the SSDP UDP source address) so a
/// renderer whose SSDP advertiser is asleep still gets discovered.
fn probe_descriptor_url(url: &str) -> Option<Renderer> {
    let (host, port, _path) = split_url(url)?;
    // Prefer the URL host parsed as a literal IP. If it's a hostname,
    // fall back to a one-shot DNS lookup (rare for Naim devices —
    // their descriptors always use IPs — but cheap insurance).
    let ip: IpAddr = host.parse().ok().or_else(|| {
        use std::net::ToSocketAddrs;
        (host.as_str(), port).to_socket_addrs().ok()
            .and_then(|mut it| it.next())
            .map(|a| a.ip())
    })?;
    fetch_renderer(url, ip)
}

// ---- helpers ---------------------------------------------------------

/// Split `http://host:port/path...` into its three pieces. Returns
/// the default port 80 when none is given.
fn split_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".to_string()));
    let (host, port) = if let Some((h, p)) = authority.split_once(':') {
        (h.to_string(), p.parse::<u16>().ok()?)
    } else {
        (authority.to_string(), 80)
    };
    Some((host, port, path))
}

/// Resolve a relative or absolute service URL against a known host /
/// port (the one we GET'd the descriptor from). UPnP device descs
/// commonly emit relative URLs.
fn absolutise(host: &str, port: u16, candidate: &str) -> String {
    if candidate.starts_with("http://") || candidate.starts_with("https://") {
        return candidate.to_string();
    }
    if candidate.starts_with('/') {
        format!("http://{host}:{port}{candidate}")
    } else {
        format!("http://{host}:{port}/{candidate}")
    }
}

/// Tiny synchronous HTTP/1.0 client. Sends a GET, reads until close,
/// returns the body (after the blank-line header terminator). Used
/// for the device-description fetch only — fine for ~10 KB XML.
fn http_get(host: &str, port: u16, path: &str, timeout: Duration) -> Option<String> {
    let addr: SocketAddr = format!("{host}:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let req = format!(
        "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\nUser-Agent: ODJ/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let split_at = text.find("\r\n\r\n").or_else(|| text.find("\n\n"))?;
    let body_start = split_at + if text[split_at..].starts_with("\r\n\r\n") { 4 } else { 2 };
    Some(text[body_start..].to_string())
}

/// First-match extractor for `<tag>contents</tag>`. Case-sensitive on
/// the tag name (UPnP XML is); strips a namespace prefix only if it's
/// in front of the tag we're after.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)?;
    Some(xml[start..start + end].trim().to_string())
}

// ---- SOAP control --------------------------------------------------
//
// UPnP `AVTransport:1` actions, fire-and-forget. Each public function
// spawns a thread so a slow / broken renderer can't hang the UI; on
// failure we log to stderr and move on. Recovery is the next state-
// transition retry (re-selecting the renderer, or the renderer
// coming back into discovery after a nap).
//
// We use `:1` rather than `:2` for the SOAPACTION header because all
// three Naim devices we've tested expose at least :1 (the Mu-so also
// has :2; the Qute and Uniti only have :1). The action names and
// argument shapes are identical between versions.

/// Tell the renderer to GET audio from `stream_url` (whose content
/// has MIME type `mime`, e.g. `audio/L16;rate=44100;channels=2`),
/// then Play. Two SOAP calls in one. Runs on a fresh thread.
///
/// The MIME goes into the `<res protocolInfo=…>` of a DIDL-Lite blob
/// inside `CurrentURIMetaData`. Naim's KnOS firmware rejects bare
/// SetAVTransportURI calls (errorCode 714: Illegal MIME-type) when
/// the metadata is empty — it can't sniff from a URL alone, so we
/// have to declare it. `audioBroadcast` is the right `upnp:class`
/// for a continuous live stream like ours.
pub fn play_url(control_url: String, stream_url: String, mime: String, label: String) {
    std::thread::Builder::new()
        .name("dj-upnp-play".into())
        .spawn(move || {
            // protocolInfo's 4th field is DLNA "additional info" —
            // semicolon-separated key=value pairs. We set:
            //   DLNA.ORG_PN=LPCM      profile name (uncompressed PCM)
            //   DLNA.ORG_OP=00        no byte-/time-based seek (live)
            //   DLNA.ORG_CI=0         no conversion indicator
            //   DLNA.ORG_FLAGS=…      flag bitfield
            // The flag bits we set:
            //   bit 24 STREAMING_TRANSFER_MODE — "live, prioritise
            //                                    low latency over
            //                                    reliability"
            //   bit 22 BACKGROUND_TRANSFER_MODE off
            //   bit 21 CONNECTION_STALLING     — sender pauses fine
            //   bit 20 DLNA_v1_5_FLAG          — speak v1.5 dialect
            // Encoded as 8 hex digits for the flags + 24 hex zero
            // pad = 32 chars total. `01700000…` is what most live
            // audio broadcasters (Shoutcast etc.) advertise; Naim
            // recognises this and uses a smaller prefetch buffer
            // than for "normal" audio streams.
            let protocol_info = format!(
                "http-get:*:{mime}:\
                 DLNA.ORG_PN=LPCM;\
                 DLNA.ORG_OP=00;\
                 DLNA.ORG_CI=0;\
                 DLNA.ORG_FLAGS=01700000000000000000000000000000"
            );
            let didl = format!(
                "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
                            xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                            xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
                   <item id=\"0\" parentID=\"-1\" restricted=\"0\">\
                     <dc:title>ODJ Master Mix</dc:title>\
                     <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
                     <res protocolInfo=\"{protocol_info}\">{stream}</res>\
                   </item>\
                 </DIDL-Lite>",
                stream = xml_escape(&stream_url),
            );
            let args = format!(
                "<InstanceID>0</InstanceID>\
                 <CurrentURI>{}</CurrentURI>\
                 <CurrentURIMetaData>{}</CurrentURIMetaData>",
                xml_escape(&stream_url),
                xml_escape(&didl),
            );
            match soap_call(&control_url, "AVTransport", "SetAVTransportURI", &args) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("upnp: SetAVTransportURI failed for {label}: {e}");
                    return;
                }
            }
            // Play — kick playback. Speed=1 is the only value Naim
            // accepts (no scrubbing / scan modes).
            let args = "<InstanceID>0</InstanceID><Speed>1</Speed>";
            if let Err(e) = soap_call(&control_url, "AVTransport", "Play", args) {
                eprintln!("upnp: Play failed for {label}: {e}");
                return;
            }
            eprintln!("upnp: playing on {label}");
        })
        .expect("spawn upnp play thread");
}

/// Stop the renderer's current playback. Best-effort.
pub fn stop(control_url: String, label: String) {
    std::thread::Builder::new()
        .name("dj-upnp-stop".into())
        .spawn(move || {
            let args = "<InstanceID>0</InstanceID>";
            if let Err(e) = soap_call(&control_url, "AVTransport", "Stop", args) {
                eprintln!("upnp: Stop failed for {label}: {e}");
                return;
            }
            eprintln!("upnp: stopped {label}");
        })
        .expect("spawn upnp stop thread");
}

/// Synchronous SOAP POST. Builds the envelope, sends it, reads the
/// response, returns the body on `2xx`. Errors fold into a single
/// String so callers can log and move on.
fn soap_call(
    control_url: &str,
    service: &str,
    action: &str,
    inner_args: &str,
) -> Result<String, String> {
    let (host, port, path) = split_url(control_url)
        .ok_or_else(|| format!("bad control URL: {control_url}"))?;
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:{action} xmlns:u="urn:schemas-upnp-org:service:{service}:1">
{inner_args}
</u:{action}>
</s:Body>
</s:Envelope>
"#
    );
    let soap_action = format!(
        "\"urn:schemas-upnp-org:service:{service}:1#{action}\""
    );
    let req = format!(
        "POST {path} HTTP/1.0\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPACTION: {soap_action}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    let addr: SocketAddr = format!("{host}:{port}").parse()
        .map_err(|e| format!("parse address: {e}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Parse just enough to know if it was 2xx. UPnP errors come back
    // as 500 with an XML fault body; we log the lot for diagnostics.
    let status_line = text.lines().next().unwrap_or("");
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .map(|c| (200..300).contains(&c))
        .unwrap_or(false);
    if !ok {
        return Err(format!("non-2xx response: {}", truncate(&text, 2000)));
    }
    let body_start = text.find("\r\n\r\n").or_else(|| text.find("\n\n"))
        .map(|i| i + if text[i..].starts_with("\r\n\r\n") { 4 } else { 2 })
        .unwrap_or(text.len());
    Ok(text[body_start..].to_string())
}

/// Escape just the XML special chars we might smuggle into a SOAP
/// argument value (URLs in particular can contain `&` for query
/// parameters). We don't ship anything fancier than a URL through
/// here, so this short list covers it.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n]) }
}

/// Walk the `<serviceList>` looking for an `AVTransport` service and
/// return its `<controlURL>`. We don't need a full XML parser for
/// this — device descriptors are well-formed and the relevant block
/// is small.
fn extract_avtransport_control_url(xml: &str) -> Option<String> {
    // Find every <service>…</service> block; pick the one whose
    // serviceType mentions AVTransport.
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("<service>") {
        let start = pos + rel;
        let end_rel = xml[start..].find("</service>")?;
        let block = &xml[start..start + end_rel];
        if block.contains(":service:AVTransport:") {
            return extract_tag(block, "controlURL");
        }
        pos = start + end_rel + "</service>".len();
    }
    None
}

// ---- tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_url_parses_explicit_port() {
        let (h, p, path) = split_url("http://192.168.68.103:8080/description.xml").unwrap();
        assert_eq!(h, "192.168.68.103");
        assert_eq!(p, 8080);
        assert_eq!(path, "/description.xml");
    }

    #[test]
    fn split_url_defaults_port_80() {
        let (_h, p, _path) = split_url("http://example.local/x").unwrap();
        assert_eq!(p, 80);
    }

    #[test]
    fn absolutise_keeps_full_urls() {
        assert_eq!(
            absolutise("h", 80, "http://other:81/y"),
            "http://other:81/y"
        );
    }

    #[test]
    fn absolutise_prepends_host_for_relative_path() {
        assert_eq!(
            absolutise("192.168.68.103", 8080, "/AVTransport/ctrl"),
            "http://192.168.68.103:8080/AVTransport/ctrl"
        );
    }

    #[test]
    fn extract_tag_finds_friendly_name() {
        let xml = "<root><friendlyName>Qute-B232 Lounge</friendlyName></root>";
        assert_eq!(extract_tag(xml, "friendlyName"), Some("Qute-B232 Lounge".to_string()));
    }

    #[test]
    fn extract_avtransport_picks_correct_service() {
        // Trimmed snippet from a real Naim UnitiQute descriptor —
        // the parser must skip ConnectionManager / RenderingControl
        // and only pick out the AVTransport block's controlURL.
        let xml = r#"<serviceList>
            <service>
                <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
                <controlURL>/RenderingControl/ctrl</controlURL>
            </service>
            <service>
                <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
                <controlURL>/ConnectionManager/ctrl</controlURL>
            </service>
            <service>
                <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
                <controlURL>/AVTransport/ctrl</controlURL>
            </service>
        </serviceList>"#;
        assert_eq!(
            extract_avtransport_control_url(xml).as_deref(),
            Some("/AVTransport/ctrl")
        );
    }

    /// Live-network test — runs a real SSDP sweep and prints the
    /// renderers found. Ignored by default so `cargo test` stays
    /// offline; run with:
    ///   cargo test -p ui --lib upnp::tests::live_scan -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_scan() {
        let found = scan_once(std::time::Duration::from_secs(4));
        for r in &found {
            println!("{:25}  {}  @ {}", r.name, r.udn, r.address);
            println!("    desc: {}", r.descriptor_url);
            println!("    AVT:  {}", r.av_transport_control);
        }
        println!("\n{} renderers found", found.len());
    }

    /// Direct-probe a known descriptor URL (skips SSDP). Use to
    /// verify the Qute-style "SSDP silent but HTTP alive" path:
    ///   cargo test -p ui --lib upnp::tests::live_probe_lounge -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_probe_lounge() {
        let url = "http://192.168.68.103:8080/description.xml";
        match probe_descriptor_url(url) {
            Some(r) => {
                println!("{:25}  {}  @ {}", r.name, r.udn, r.address);
                println!("    AVT: {}", r.av_transport_control);
            }
            None => println!("no response from {url}"),
        }
    }

    /// Live SOAP smoke — direct-probe the Lounge, send it
    /// SetAVTransportURI + Play pointing at a known internet stream,
    /// wait, then Stop. If it works the Naim plays the BBC Radio 1
    /// MP3 stream briefly. Validates the SOAP envelope shape /
    /// SOAPACTION header on real hardware before we wire it into
    /// the app. Synchronous (calls `soap_call` directly) so we see
    /// the response inline.
    ///   cargo test -p ui --lib upnp::tests::live_soap_play -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_soap_play() {
        let ctrl = "http://192.168.68.103:8080/AVTransport/ctrl";
        let stream = "http://stream.live.vc.bbcmedia.co.uk/bbc_radio_one";
        let didl = format!(
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
                        xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                        xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
               <item id=\"0\" parentID=\"-1\" restricted=\"0\">\
                 <dc:title>Live SOAP test</dc:title>\
                 <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
                 <res protocolInfo=\"http-get:*:audio/mpeg:*\">{}</res>\
               </item>\
             </DIDL-Lite>",
            xml_escape(stream),
        );
        let set = format!(
            "<InstanceID>0</InstanceID>\
             <CurrentURI>{}</CurrentURI>\
             <CurrentURIMetaData>{}</CurrentURIMetaData>",
            xml_escape(stream),
            xml_escape(&didl),
        );
        match soap_call(ctrl, "AVTransport", "SetAVTransportURI", &set) {
            Ok(b) => println!("SetAVTransportURI OK\n{}", truncate(&b, 400)),
            Err(e) => { println!("SetAVTransportURI ERR: {}", truncate(&e, 2000)); return; }
        }
        match soap_call(ctrl, "AVTransport", "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>") {
            Ok(b) => println!("Play OK\n{}", truncate(&b, 400)),
            Err(e) => { println!("Play ERR: {e}"); return; }
        }
        println!("(playing for 6 s — if the Lounge is on you should hear Radio 1)");
        std::thread::sleep(Duration::from_secs(6));
        match soap_call(ctrl, "AVTransport", "Stop", "<InstanceID>0</InstanceID>") {
            Ok(_) => println!("Stop OK"),
            Err(e) => println!("Stop ERR: {e}"),
        }
    }
}
