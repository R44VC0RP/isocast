# Isocast's pinned Slint surface hook

Source: crates.io `i-slint-renderer-skia` 1.18.1, as pinned by Cargo.lock.
Original crate SHA-256: `dc9c1cf02385f1d26fc1f423544e52390a8e5c2d89420a8042aa94b4fe109d1f`.
Licensing is unchanged; Isocast uses Slint's Royalty-free licence. Upstream
licence texts and source notices are retained.

Only `wgpu_30_surface.rs` differs from upstream. A Windows-only, thread-local
factory lets an application using Manual WGPU configuration supply a surface,
initial alpha mode and a post-configure callback. Isocast uses it to put Slint's
premultiplied swapchain above its DirectComposition preview visuals. Alpha must
be selected before first configure: DX12 ResizeBuffers cannot change it later.
The callback commits the application-owned composition tree after attachment
and reconfiguration. No renderer, input, layout or other-platform changes.
