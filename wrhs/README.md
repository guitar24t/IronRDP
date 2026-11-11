# WRHS branch

This branch of IronRDP adds what WRHS (a remote desktop that runs IronRDP's web client
inside an end-to-end encrypted tunnel) needs for video and sound in the browser:

- **Graphics Pipeline in the web client** (`crates/ironrdp-web/src/gfx.rs`), with H.264
  decoded by the page (for example with WebCodecs). Browsers decode asynchronously, so the
  pipeline runs outside the session (as `ironrdp_session::composite_graphics_pipeline`
  describes) and decodes each message's H.264 before processing it. For that,
  `GraphicsPipelineClient` gains `decode_messages` and `process_pdu`.
  Extensions: `graphicsPipeline(true)` and `h264Decoder(factory)`.
- **Audio output in the web client** (`crates/ironrdp-web/src/audio.rs`): RDPSND hands
  16-bit PCM to the page. Extension: `audioPlayback(callbacks)`.
- `wrhs/build-web-package.sh`: builds `@devolutions/iron-remote-desktop-rdp` (one ES module
  with the WebAssembly inlined) into `wrhs/dist`.
- `wrhs/test-server`: an RDP test server that streams an H.264 test pattern and a 440 Hz
  tone, for end-to-end tests of web clients.

Built with the stable Rust toolchain (`crates/ironrdp-web/rust-toolchain.toml`), wasm-pack
0.15.0 and Node.js 22 or later.
