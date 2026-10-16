//! Builds libvpx's VP8 decoder (and nothing else of libvpx) as a static
//! library from the sources vendored in `libvpx/`.
//!
//! Provenance: libvpx v1.17.0 (https://chromium.googlesource.com/webm/libvpx,
//! commit 6df3ec34557879fff673706f4a1d9fbd0f3a6f0e), BSD-3-Clause. The
//! headers in `libvpx/config/` and the file list below are what libvpx's own
//! build generates and compiles for
//!
//!   configure --target=arm64-darwin23-gcc --disable-vp9 --disable-vp8-encoder
//!     --enable-vp8-decoder --disable-examples --disable-tools --disable-docs
//!     --disable-unit-tests --enable-multithread --disable-webm-io
//!     --disable-libyuv --size-limit=16384x16384 --disable-runtime-cpu-detect
//!     --disable-neon-dotprod --disable-neon-i8mm --disable-sve --disable-sve2
//!
//! (NEON is in every Apple silicon Mac, so its routines are chosen at compile
//! time.) Only those sources and the headers they include are vendored,
//! unchanged. To update: run that configure in an empty folder, `make
//! V=1 libvpx.a`, and copy the compiled .c files, the headers in their .d
//! files, LICENSE, PATENTS and AUTHORS.

use std::env;

const SOURCES: &[&str] = &[
    "vp8/common/alloccommon.c",
    "vp8/common/arm/loopfilter_arm.c",
    "vp8/common/arm/neon/bilinearpredict_neon.c",
    "vp8/common/arm/neon/copymem_neon.c",
    "vp8/common/arm/neon/dc_only_idct_add_neon.c",
    "vp8/common/arm/neon/dequant_idct_neon.c",
    "vp8/common/arm/neon/dequantizeb_neon.c",
    "vp8/common/arm/neon/idct_blk_neon.c",
    "vp8/common/arm/neon/iwalsh_neon.c",
    "vp8/common/arm/neon/loopfiltersimplehorizontaledge_neon.c",
    "vp8/common/arm/neon/loopfiltersimpleverticaledge_neon.c",
    "vp8/common/arm/neon/mbloopfilter_neon.c",
    "vp8/common/arm/neon/shortidct4x4llm_neon.c",
    "vp8/common/arm/neon/sixtappredict_neon.c",
    "vp8/common/arm/neon/vp8_loopfilter_neon.c",
    "vp8/common/blockd.c",
    "vp8/common/dequantize.c",
    "vp8/common/entropy.c",
    "vp8/common/entropymode.c",
    "vp8/common/entropymv.c",
    "vp8/common/extend.c",
    "vp8/common/filter.c",
    "vp8/common/findnearmv.c",
    "vp8/common/generic/systemdependent.c",
    "vp8/common/idct_blk.c",
    "vp8/common/idctllm.c",
    "vp8/common/loopfilter_filters.c",
    "vp8/common/mbpitch.c",
    "vp8/common/modecont.c",
    "vp8/common/quant_common.c",
    "vp8/common/reconinter.c",
    "vp8/common/reconintra.c",
    "vp8/common/reconintra4x4.c",
    "vp8/common/rtcd.c",
    "vp8/common/setupintrarecon.c",
    "vp8/common/swapyv12buffer.c",
    "vp8/common/treecoder.c",
    "vp8/common/vp8_loopfilter.c",
    "vp8/decoder/dboolhuff.c",
    "vp8/decoder/decodeframe.c",
    "vp8/decoder/decodemv.c",
    "vp8/decoder/detokenize.c",
    "vp8/decoder/onyxd_if.c",
    "vp8/decoder/threading.c",
    "vp8/vp8_dx_iface.c",
    "vpx_dsp/arm/intrapred_neon.c",
    "vpx_dsp/bitreader_buffer.c",
    "vpx_dsp/bitreader.c",
    "vpx_dsp/intrapred.c",
    "vpx_dsp/prob.c",
    "vpx_dsp/skin_detection.c",
    "vpx_dsp/vpx_dsp_rtcd.c",
    "vpx_mem/vpx_mem.c",
    "vpx_ports/aarch64_cpudetect.c",
    "vpx_scale/generic/gen_scalers.c",
    "vpx_scale/generic/vpx_scale.c",
    "vpx_scale/generic/yv12config.c",
    "vpx_scale/generic/yv12extend.c",
    "vpx_scale/vpx_scale_rtcd.c",
    "vpx_util/vpx_thread.c",
    "vpx_util/vpx_write_yuv_frame.c",
    "vpx/src/vpx_codec.c",
    "vpx/src/vpx_decoder.c",
    "vpx/src/vpx_encoder.c",
    "vpx/src/vpx_image.c",
    "config/vpx_config.c",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=libvpx");
    let target = env::var("TARGET").unwrap_or_default();
    // The vendored configuration is libvpx's for arm64 macOS (NEON); the
    // crate is empty on other targets.
    if target != "aarch64-apple-darwin" {
        return;
    }
    let mut build = cc::Build::new();
    build
        .include("libvpx/config")
        .include("libvpx")
        .files(SOURCES.iter().map(|source| format!("libvpx/{source}")))
        .define("NDEBUG", None)
        // Optimised even in debug builds (the dev app decodes in real time).
        .opt_level(3)
        .flag("-std=gnu11")
        .flag("-march=armv8-a")
        // libvpx's own code; its warnings aren't ours to fix.
        .warnings(false)
        .flag("-w");
    build.compile("isocast_vpx");
}
