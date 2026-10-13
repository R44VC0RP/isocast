# GPU path research (for tasks 3.1–3.6)

Research done 2026-10-11 from crate sources (wgpu 30.0.1, wgpu-hal 30.0.1,
objc2-core-video 0.3.2, objc2-metal 0.3.2, png 0.18.1). Nothing here was
built yet; verify as the tasks are done.

## Crates

- `wgpu = { version = "=30.0.1", default-features = false, features = ["std", "parking_lot", "metal", "wgsl"] }`.
  `wgpu::hal` is re-exported. wgpu-hal 29+ uses objc2-metal 0.3 / objc2 0.6,
  the same family as the rest of the repo (≤28 used `metal`-rs; most examples
  online are for that). Pin the version: hal APIs change between releases.
- objc2-core-video features `CVMetalTexture`, `CVMetalTextureCache`,
  `objc2-metal`; objc2-metal features `MTLDevice`, `MTLTexture`,
  `MTLPixelFormat`, `MTLResource`.
- `pollster` (or `Waker::noop()`) for the adapter/device futures; `png` for
  snapshots.

## Device and wrapping Metal textures

```rust
let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
    backends: wgpu::Backends::METAL, ..wgpu::InstanceDescriptor::new_without_display_handle() });
let adapter = pollster::block_on(instance.request_adapter(&Default::default()))?;
let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
    required_features: wgpu::Features::TEXTURE_FORMAT_16BIT_NORM, // for 'x420'
    ..Default::default() }))?;
let mtl = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.unwrap().raw_device().clone();
```

An existing MTLTexture becomes a wgpu texture in two steps:
`wgpu::hal::metal::Device::texture_from_raw(raw, format, MTLTextureType::Type2D, 1, 1, CopyExtent{..}, drop_callback)`
then `device.create_texture_from_hal::<hal::api::Metal>(hal_tex, &desc, initial_state)`
(`initial_state`: `TextureUses::RESOURCE` for inputs, `COLOR_TARGET` for
outputs). `Queue::as_hal` gives the MTLCommandQueue and shared-event waits.

## CVPixelBuffer → MTLTexture without copying

One `CVMetalTextureCache` per device; per plane
`CVMetalTextureCache::create_texture_from_image(None, &cache, pb, attrs, mtl_fmt, w, h, plane, out)`,
then `CVMetalTextureGetTexture`. Use the plane's own width/height (never w/2:
odd sizes). Keep the CVMetalTexture and the frame alive in the hal texture's
drop callback: wgpu-core frees hal textures only after the submissions using
them complete, so lifetimes are exact. Call `device.poll(PollType::Poll)` and
`cache.flush(0)` every frame, and don't keep textures across frames (that
starves capture pools; ScreenCaptureKit's queue depth is 8).

| CV format | plane 0 | plane 1 |
|---|---|---|
| `BGRA` | `Bgra8Unorm` | — |
| `420v` / `420f` | `R8Unorm` | `Rg8Unorm` |
| `x420` | `R16Unorm` | `Rg16Unorm` (10 bits in the top of 16) |

## YUV → RGB (one shader, matrix as a uniform)

```wgsl
let ycc = vec3(textureSample(y_tex, s, uv).r, textureSample(c_tex, s, uv).rg);
return vec4(clamp(csc.m * (ycc - csc.off), vec3(0.0), vec3(1.0)), 1.0);
```

BT.709 (rows R, G, B; columns Y, Cb, Cr; WGSL is column-major, so transpose):

| | offset Y / C | Y | R←Cr | G←Cb | G←Cr | B←Cb |
|---|---|---|---|---|---|---|
| 420v | 0.062745 / 0.501961 | 1.164384 | 1.792741 | −0.213249 | −0.532909 | 2.112402 |
| x420 | 0.062501 / 0.500008 | 1.168932 | 1.799744 | −0.214082 | −0.534991 | 2.120653 |
| 420f | 0 / 0.501961 | 1 | 1.5748 | −0.187324 | −0.468124 | 1.8556 |

Pick the matrix from the buffer's `kCVImageBufferYCbCrMatrixKey` attachment
(default 709).

## Canvas output

Render into an IOSurface-backed BGRA CVPixelBuffer from a pool (add
`kCVPixelBufferMetalCompatibilityKey`), imported with
`kCVMetalTextureUsage = ShaderRead | RenderTarget`, wgpu usage
`RENDER_ATTACHMENT | TEXTURE_BINDING | COPY_SRC`; that buffer goes straight
to AVAssetWriter. Better for 3.3: a final pass into a `420v`/`x420` buffer (Y
into the R8 plane, CbCr at half size into RG8), the encoder's native input.

## Pitfalls

- Declared wgpu usage must match the MTLTexture's usage; check with
  `MTL_DEBUG_LAYER=1` and `InstanceFlags::VALIDATION`.
- Keep the view format equal to the texture format; use `Bgra8Unorm`, not
  `…Srgb` (stay in 709 gamma).
- No `clear_texture` on imported textures (use `LoadOp::Clear`).
- Don't hand the output buffer to the writer before the GPU finished:
  `device.poll(PollType::Wait { submission_index, .. })` or
  `on_submitted_work_done`; pipeline 2–3 frames.
- IOSurface textures are not flipped: origin top-left, as in wgpu.

## Snapshots

The canvas is a CVPixelBuffer in shared memory: once its submission is done,
lock it read-only, copy rows (BGRA → RGB) and write a PNG with
`png::Encoder` (`Rgb`, 8-bit, fast compression). General fallback:
`copy_texture_to_buffer` (256-byte row padding), `map_async`, poll. Expose it
as a daemon method so the CLI and MCP `preview_frame` share it.
