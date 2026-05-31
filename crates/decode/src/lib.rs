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
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

/// Tags extracted from a file's metadata header (ID3v2, Vorbis comments,
/// mp4 atoms…). Each field is `None` when the file doesn't carry that tag.
#[derive(Debug, Default, Clone)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
}

/// Read the audio file's metadata header without decoding samples. Fast —
/// just probes the format and walks the tag table. Used by the track
/// scanner to populate the picker's columns + filters.
pub fn read_tags(path: impl AsRef<Path>) -> Result<TrackTags> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .context("symphonia probe failed")?;

    let mut tags = TrackTags::default();
    // Some formats carry metadata in the probe's outer container (e.g. ID3v2
    // sitting in front of an MP3); others carry it on the format itself
    // (e.g. FLAC Vorbis comments, mp4 atoms). Walk both.
    if let Some(meta) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        absorb_tags(&mut tags, meta.tags());
    }
    if let Some(meta) = probed.format.metadata().current() {
        absorb_tags(&mut tags, meta.tags());
    }
    Ok(tags)
}

fn absorb_tags(out: &mut TrackTags, tags: &[symphonia::core::meta::Tag]) {
    for t in tags {
        let Some(std_key) = t.std_key else { continue };
        let slot = match std_key {
            StandardTagKey::TrackTitle if out.title.is_none() => &mut out.title,
            StandardTagKey::Artist if out.artist.is_none() => &mut out.artist,
            StandardTagKey::Album if out.album.is_none() => &mut out.album,
            StandardTagKey::Genre if out.genre.is_none() => &mut out.genre,
            _ => continue,
        };
        if let Value::String(s) = &t.value {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                *slot = Some(trimmed.to_string());
            }
        }
    }
}

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
