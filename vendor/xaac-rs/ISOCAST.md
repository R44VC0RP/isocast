# xaac-rs 0.2.0

Published crates.io source, Apache-2.0 (author attribution in Cargo.toml).
The publisher's reported revision (marked dirty at publication) is recorded
in `.cargo_vcs_info.json`; the source baseline is the published crate archive.
The published archive ships no licence file: LICENSE-APACHE (the Apache
License 2.0 text) was added by Isocast to accompany the manifest's
`license = "Apache-2.0"`.

One portability fix in src/util.rs: cast libxaac's `WORD8` (`i8`) string
pointer to platform `c_char` before CStr::from_ptr. On Linux aarch64,
`c_char` is `u8`, and unpatched 0.2.0 fails to compile with E0308. The byte
representation and NUL-terminated string handling are unchanged.

Trimmed to the native AAC encoder Isocast uses: Python bindings and their
optional PyO3 feature/dependency, Python/Rust examples, upstream test target,
`Cargo.toml.orig` and the Python-oriented cdylib target are omitted, and so
is the native decoder wrapper (src/decoder.rs, its re-exports from
src/lib.rs, and the decoder-only helpers it alone used: `check_decoder` and
`decoder_version` in src/ffi.rs, `AlignedBuffer` in src/util.rs). The shared
`Error` enum is kept unchanged, including its decoder variants. The native
encoder, FFI, error and allocation code is otherwise as published, together
with the upstream README (which still describes the omitted decoder and
Python bindings) and revision metadata. libxaac itself, including its
decoder sources, comes unmodified from libxaac-sys. Isocast's native codec
and independent-decoder tests live in isocast-platform-linux instead.
