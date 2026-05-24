# Persistence: favourites + analysis cache

Implementation lives in `crates/ui/src/persistence.rs`.

## Files

Both in the music directory:

- **`.favourites`** — one absolute path per line. Toggled via `★`
  button per row; rewritten on every toggle (small, hundreds of
  entries max).
- **`.analysis-cache`** — one entry per line, pipe-delimited:
  `path|bpm|tonic|is_minor|beat0,beat1,…`. Appended on each new
  analysis. `tonic = -1` means key undetected.

## Why line-based not XML/JSON

Zero new dependencies, trivially diffable, append-on-completion =
atomic per-entry. Pipes are uncommon in audio filenames; if a path
contains one the write is skipped rather than risk corrupting the
file. (A "single XML file for all of it" was floated and rejected on
those grounds — same shape, simpler implementation.)

## Background analysis worker

Spawned once at startup in `DjApp::new`. Iterates the music directory
listing, skips anything already in cache, decodes + runs
`analysis::analyse`, inserts into cache (in-memory + file append).
Progress is exposed via `Arc<AtomicUsize>` and shown in the top bar
(`"analysing N/total"`). UI stays responsive throughout — worker is its
own thread, cpal still runs on its own, egui on the main thread.

## On-load fast path

`start_load` checks the cache first; if hit, skips re-analysis and uses
the cached bpm/key/beats. Cold start still does on-load analysis and
writes back to cache so the second session is hot.

## Filters in the side panel

- Text filter (substring match on title/artist/filename).
- `★ only` checkbox — show only favourited tracks.
- `Compat: off / Deck A / Deck B` combo — show only tracks whose cached
  key is *harmonically compatible* with the deck's loaded track.
  Compatible = same key, relative major/minor (same number, different
  letter), or ±1 number same letter (perfect fifth) on the Camelot
  wheel. Implemented in `persistence::camelot_compatible`. Tracks with
  no cached key are excluded when this filter is active.

## Known limits

- No cache invalidation when a file is modified — file mtime isn't
  tracked. If a track is replaced with the same name but different
  content, the cache keeps the old analysis until the
  `.analysis-cache` entry is removed by hand.
- No per-entry analysis_version bookkeeping in the cache yet, so
  future analyser changes won't auto-invalidate. Bump by deleting the
  file.
- Worker is serial (one decode + analysis at a time). Could parallelise
  to `num_cpus / 2` workers for faster cold starts; not worth the
  complexity yet.
