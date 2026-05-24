# Build sequence (historical)

Original three-step plan agreed at the start of the project:

1. **Throwaway latency spike** to prove cpal can deliver low-latency
   audio output on this PipeWire box. Not committed to git.
2. **Design doc** — architecture, module boundaries, `DeckCommand`
   enum, MIDI mapping schema. Written after the spike confirmed the
   audio approach was viable.
3. **Real scaffold** — Cargo workspace, single-deck play/pause
   working end-to-end via keyboard + MIDI.

The spike directory (`spike/`) is gitignored.

**Why this order:** validate that low-latency Rust audio actually
worked on the dev machine before investing in design. If cpal latency
had been unacceptable through PipeWire, the whole plan would have
needed to change.

**How to apply for future significant features:** don't propose
architecture beyond what the current phase needs. Spike first if
you're not sure the approach is viable.
