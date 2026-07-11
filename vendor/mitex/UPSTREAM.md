# Vendored MiTeX scope

- Upstream: https://github.com/mitex-rs/mitex
- Commit: `985d8e725922ceb70ae5459c50c4cb3d733a0ed1`
- Rust crate: `mitex = 0.2.4`
- License: Apache-2.0 (see `LICENSE`)

The files under `specs/` are embedded into the binary and installed next to the
generated Typst document. The WebAssembly plugin is intentionally not vendored.
The crate and these scope files must be upgraded together.
