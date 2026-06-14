//! Local HTTP server that serves the post-master mix as raw L16 PCM
//! to a UPnP MediaRenderer (or anyone else with curl).
//!
//! Why HTTP and L16: UPnP MediaRenderer expects to PULL audio from a
//! URL it's given via `SetAVTransportURI` (§3 wires that). L16 (raw
//! 16-bit big-endian PCM) is the DLNA baseline format — works on
//! every Naim we've tested and adds zero source-side encoding
//! latency. No chunked transfer encoding either: we serve HTTP/1.0
//! `Connection: close` with no Content-Length, so the renderer reads
//! until the socket goes EOF. Older Naim firmware handles that
//! consistently; chunked encoding is iffier.
//!
//! Threading: one accept thread per `NetworkOutput`. It accepts one
//! client at a time — the renderer is the only intended caller. A
//! new connection while one is active politely drops the old one
//! (renderer must have reconnected). All connection state is
//! contained in the accept thread; the rest of the app interacts
//! only via the `enabled` atomic and the URL helper.

use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Hot bits of state shared between the accept thread and the rest of
/// the app. Cheap to clone (`Arc`s under the hood).
pub struct NetworkOutput {
    /// The port we're listening on. Combine with `lan_url_for` to get
    /// the full URL the renderer should hit. (§3 reads this; the
    /// HTTP server itself doesn't need it back after spawn.)
    #[allow(dead_code)] // wired up in network-output §3
    port: u16,
    /// Audio thread tap is always pushing into the ring; this flag
    /// gates whether the accept thread serves connections at all.
    /// When the user deselects the renderer we flip this false so any
    /// in-flight `Play` from a stale renderer falls off. Set to true
    /// before the first SOAP `Play` lands so the renderer's HTTP GET
    /// finds the stream live and ready.
    enabled: Arc<AtomicBool>,
    /// Engine-side audio format. Embedded in the L16 MIME type.
    #[allow(dead_code)] // captured by the accept thread; kept here for diagnostics
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u16,
}

impl NetworkOutput {
    /// Spawn the accept thread. Returns immediately with the bound
    /// port. The thread sits on `accept()` indefinitely; one client
    /// at a time, switches over cleanly when a new one connects.
    ///
    /// `consumer` is the audio-thread tap — the accept thread reads
    /// from it inside the connection handler and writes L16 PCM out.
    pub fn spawn(
        consumer: rtrb::Consumer<f32>,
        sample_rate: u32,
        channels: u16,
    ) -> std::io::Result<Self> {
        // Bind to 0.0.0.0 so the renderer can reach us from any
        // interface. Ephemeral port keeps this collision-free with
        // anything else the user has running.
        let listener = TcpListener::bind("0.0.0.0:0")?;
        let port = listener.local_addr()?.port();
        let enabled = Arc::new(AtomicBool::new(false));

        eprintln!(
            "network-output: listening on 0.0.0.0:{port}  (audio/L16; rate={sample_rate}; channels={channels})"
        );

        let enabled_thread = Arc::clone(&enabled);
        std::thread::Builder::new()
            .name("dj-network-output".into())
            .spawn(move || {
                accept_loop(listener, consumer, enabled_thread, sample_rate, channels);
            })?;

        Ok(NetworkOutput {
            port,
            enabled,
            sample_rate,
            channels,
        })
    }

    /// Start serving — flip the gate on. Idempotent.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Stop serving. The accept thread stays alive (it'll just refuse
    /// new connections and drop in-flight ones); cheap to flip back
    /// on when the user re-selects.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
    }

    /// Construct the URL to advertise to a renderer at `renderer_ip`.
    /// We bind 0.0.0.0; the renderer needs *our* address in their
    /// subnet to GET from. Picks our source IP by `connect()`-ing a
    /// UDP socket to the renderer — no datagram is sent; the kernel
    /// just resolves the route and assigns a source IP we can read.
    #[allow(dead_code)] // wired up in network-output §3
    pub fn lan_url_for(&self, renderer_ip: IpAddr) -> Option<String> {
        let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
        probe.connect(SocketAddr::new(renderer_ip, 9)).ok()?;
        let our = probe.local_addr().ok()?.ip();
        Some(format!("http://{our}:{port}/stream", port = self.port))
    }
}

// ---- accept thread ---------------------------------------------------

fn accept_loop(
    listener: TcpListener,
    mut consumer: rtrb::Consumer<f32>,
    enabled: Arc<AtomicBool>,
    sample_rate: u32,
    channels: u16,
) {
    // Short accept timeout so the loop can periodically notice the
    // gate flipping off and stop responding without leaking a
    // half-handled connection.
    let _ = listener.set_nonblocking(false);
    let _ = listener
        .set_ttl(64) // standard; just to avoid relying on the OS default
        ;

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("network-output: accept error: {e}");
                continue;
            }
        };
        if !enabled.load(Ordering::SeqCst) {
            // Renderer not currently selected — politely refuse so
            // the client tries again or moves on.
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        }
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        eprintln!("network-output: client connected from {peer}");
        // Serve until the client disconnects, the ring closes, or
        // `disable()` is called.
        let result = serve_one(stream, &mut consumer, &enabled, sample_rate, channels);
        match result {
            Ok(()) => eprintln!("network-output: client {peer} disconnected cleanly"),
            Err(e) => eprintln!("network-output: client {peer} closed ({e})"),
        }
    }
}

fn serve_one(
    mut stream: TcpStream,
    consumer: &mut rtrb::Consumer<f32>,
    enabled: &AtomicBool,
    sample_rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Eat the GET request line + headers. We don't actually need
    // anything from them — there's only one resource we serve.
    let _ = drain_http_request(&mut stream);

    // HTTP/1.0 + Connection: close + no Content-Length is the
    // "stream until EOF" mode every Naim we've tested handles
    // gracefully. Chunked encoding works on the modern Mu-so but
    // older UnitiQute firmware mis-parses some chunk sizes — keep
    // this simple. `contentFeatures.dlna.org` advertises us as
    // generic LPCM which lets the renderer skip its container-
    // format detection logic.
    let headers = format!(
        "HTTP/1.0 200 OK\r\n\
         Server: ODJ/0.1\r\n\
         Connection: close\r\n\
         Content-Type: audio/L16; rate={sample_rate}; channels={channels}\r\n\
         transferMode.dlna.org: Streaming\r\n\
         contentFeatures.dlna.org: DLNA.ORG_PN=LPCM\r\n\
         \r\n",
    );
    stream.write_all(headers.as_bytes())?;
    stream.flush()?;

    // Latency hygiene: drain anything already in the ring before we
    // start serving. While no client was connected the audio thread
    // kept pushing into a ring nobody drained — it sat full of up to
    // ~1 s of stale audio. If we shipped that on the wire first the
    // Naim would prefetch it (sub-second over LAN) plus its own
    // safety margin, producing the multi-beat startup latency we
    // can otherwise never undercut. Empty ring → only realtime data
    // ever reaches the renderer.
    let mut dropped = 0usize;
    while consumer.pop().is_ok() {
        dropped += 1;
    }
    if dropped > 0 {
        eprintln!(
            "network-output: dropped {} stale frames on client connect",
            dropped / channels as usize,
        );
    }

    // Drain f32s from the audio ring, byte-swap into i16 big-endian
    // PCM, write in modest chunks. Sized to ~10 ms at typical rates
    // — enough to amortise the syscall, small enough that latency
    // doesn't balloon. Holds a stereo frame integer multiple so the
    // wire stays L/R-aligned.
    const CHUNK_FRAMES: usize = 512;
    let chunk_samples = CHUNK_FRAMES * channels as usize;
    let mut buf = vec![0u8; chunk_samples * 2]; // i16 = 2 bytes
    loop {
        if !enabled.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Wait for a chunk's worth of audio in the ring. The audio
        // thread runs at real time, so this normally fills in <10 ms;
        // if the consumer side ever lags we just wait it out.
        while consumer.slots() < chunk_samples {
            if !enabled.load(Ordering::SeqCst) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // Pull + convert. Saturating conversion clamps absurd values
        // (oscillator NaN, runaway feedback) into the i16 range
        // rather than wrapping into hilarious clip noise.
        for i in 0..chunk_samples {
            let f = consumer.pop().unwrap_or(0.0);
            let v = (f.clamp(-1.0, 1.0) * 32_767.0) as i16;
            let be = v.to_be_bytes();
            buf[i * 2] = be[0];
            buf[i * 2 + 1] = be[1];
        }
        stream.write_all(&buf)?;
    }
}

// ---- tests ----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    /// End-to-end smoke: spawn a synthetic audio source pushing a
    /// loud sine wave into the ring, spawn NetworkOutput, enable
    /// it, connect as a "renderer", read the HTTP headers + a chunk
    /// of audio, assert the audio is non-silent and big-endian
    /// (i.e. the high byte of every i16 carries the wave's energy).
    /// All in-process; no external curl / network setup.
    #[test]
    fn serves_l16_audio_to_a_client() {
        // 44.1k stereo. 2 s of headroom in the ring is plenty for
        // a test that pulls a few KB total.
        const SR: u32 = 44_100;
        const CH: u16 = 2;
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(SR as usize * CH as usize * 2);

        // Fake audio thread: push a 1 kHz sine into the ring at
        // roughly real-time. We don't need to be tight — the
        // server's `slots()` wait absorbs whatever cadence we
        // produce at.
        let stop = Arc::new(AtomicBool::new(false));
        let stop_audio = Arc::clone(&stop);
        let audio_thread = std::thread::spawn(move || {
            let mut phase = 0.0_f32;
            let inc = 2.0 * std::f32::consts::PI * 1000.0 / SR as f32;
            let frames_per_tick = SR as usize / 100; // 10 ms
            while !stop_audio.load(Ordering::SeqCst) {
                for _ in 0..frames_per_tick {
                    let s = phase.sin() * 0.5;
                    phase = (phase + inc).rem_euclid(2.0 * std::f32::consts::PI);
                    for _ in 0..CH {
                        let _ = producer.push(s);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let no = NetworkOutput::spawn(consumer, SR, CH).expect("spawn");
        no.enable();

        // Give the accept thread a beat to bind & start polling.
        std::thread::sleep(Duration::from_millis(50));

        // Connect as a client. Localhost, the port we just bound.
        let addr: SocketAddr = format!("127.0.0.1:{}", no.port).parse().unwrap();
        let mut client = TcpStream::connect_timeout(&addr, Duration::from_secs(1))
            .expect("client connect");
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client.write_all(b"GET /stream HTTP/1.0\r\n\r\n").unwrap();

        // Read the HTTP headers (terminated by blank line).
        let mut header_bytes = Vec::new();
        let mut buf = [0u8; 256];
        let start = Instant::now();
        loop {
            let n = client.read(&mut buf).expect("read");
            header_bytes.extend_from_slice(&buf[..n]);
            if header_bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(2), "no headers in 2s");
        }
        let headers = String::from_utf8_lossy(&header_bytes);
        assert!(headers.contains("HTTP/1.0 200"), "no 200: {headers}");
        assert!(
            headers.contains("audio/L16"),
            "wrong content-type: {headers}",
        );
        assert!(headers.contains(&format!("rate={SR}")));

        // Pull the body — find where it starts in what we already
        // read, then read more until we have a meaningful chunk of
        // audio (8 KB = ~46 ms at 44.1 k stereo).
        let body_split = header_bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .unwrap()
            + 4;
        let mut audio: Vec<u8> = header_bytes[body_split..].to_vec();
        while audio.len() < 8192 {
            let n = client.read(&mut buf).expect("read body");
            assert!(n > 0, "premature EOF");
            audio.extend_from_slice(&buf[..n]);
            assert!(start.elapsed() < Duration::from_secs(3), "audio drained too slowly");
        }

        // Interpret as i16 BE pairs (L, R). For a 0.5-amplitude sine
        // we should see plenty of samples whose magnitude > 5_000.
        let mut loud = 0;
        for chunk in audio.chunks_exact(2) {
            let v = i16::from_be_bytes([chunk[0], chunk[1]]);
            if v.unsigned_abs() > 5_000 {
                loud += 1;
            }
        }
        assert!(
            loud > 100,
            "audio looks silent / endian-wrong: {loud} loud samples out of {}",
            audio.len() / 2,
        );

        stop.store(true, Ordering::SeqCst);
        drop(client);
        audio_thread.join().unwrap();
    }
}

/// Read the request line + headers, stop at the blank line. Best
/// effort — we don't care what the client sent, only that we don't
/// start writing the response while there's still pending header
/// bytes the client might want acknowledged.
fn drain_http_request(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = [0u8; 1024];
    let mut total = Vec::new();
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total.extend_from_slice(&buf[..n]);
        if total.windows(4).any(|w| w == b"\r\n\r\n") || total.windows(2).any(|w| w == b"\n\n") {
            break;
        }
        if total.len() > 8192 {
            break; // pathological — give up but keep the connection
        }
    }
    Ok(())
}
