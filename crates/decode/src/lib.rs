//! Audio decode via symphonia. v1 ships with wav + pcm features; other
//! formats (mp3, flac, aac) are a feature-flag flip in workspace Cargo.toml.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use control::TrackBuffer;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Fully decode the given audio file into a `TrackBuffer` (interleaved f32).
pub fn load_to_buffer(path: impl AsRef<Path>) -> Result<Arc<TrackBuffer>> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("symphonia probe failed")?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no decodable tracks in file"))?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params
        .sample_rate
        .ok_or_else(|| anyhow!("unknown sample rate"))?;
    let channels = codec_params
        .channels
        .ok_or_else(|| anyhow!("unknown channel layout"))?
        .count() as u16;

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("no decoder for codec")?;

    let mut samples = Vec::<f32>::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(e).context("reading packet"),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                if sample_buf.is_none() {
                    let spec = *audio_buf.spec();
                    sample_buf = Some(SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec));
                }
                if let Some(sb) = sample_buf.as_mut() {
                    sb.copy_interleaved_ref(audio_buf);
                    samples.extend_from_slice(sb.samples());
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(e)) => {
                // Per symphonia docs: DecodeError is recoverable per-packet.
                eprintln!("decode: skipping bad packet ({e})");
                continue;
            }
            Err(e) => return Err(e).context("decoding"),
        }
    }

    Ok(Arc::new(TrackBuffer {
        samples,
        channels,
        sample_rate,
    }))
}
