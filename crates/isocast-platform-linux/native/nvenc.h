#ifndef ISOCAST_LINUX_NVENC_H
#define ISOCAST_LINUX_NVENC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct IcNvenc IcNvenc;

/* Single caller at a time, but calls may move between threads. Native waits
 * (including close) have no deadline. No CUDA toolkit or FFmpeg is required.
 * Device 0 is used; it need not match the compositor GPU because input is a
 * CPU copy, not an imported GPU resource. Adapter selection is not exposed.
 *
 * Dimensions must be even, >=16, and fit 3840x2160 or 2160x3840; fps is 1..60.
 * gop is in frames (UINT32_MAX means infinite). qp=UINT32_MAX selects bitrate
 * control (cbr=0: capped VBR, cbr=1: CBR); otherwise qp must be 0..51.
 * QP 0 is not lossless H.264 High. kbps is ignored in fixed-QP mode.
 * open returns NULL on failure; other fallible functions return 0 or -1.
 * Errors are NUL-terminated when error != NULL and error_cap > 0.
 */
IcNvenc *ic_nvenc_open(uint32_t width, uint32_t height, uint32_t fps,
                      uint32_t kbps, uint32_t gop, uint32_t qp, uint32_t cbr,
                      char *error, size_t error_cap);

/* Annex-B SPS/PPS are available immediately after open, before any frames.
 * On insufficient capacity, written is the required size; headers may be
 * retried. All other output lengths are valid only on success.
 */
int ic_nvenc_headers(IcNvenc *encoder, uint8_t *out, size_t capacity,
                     size_t *written, char *error, size_t error_cap);

/* Exactly width*height*3/2 tightly packed NV12 bytes, limited-range BT.709.
 * Success produces exactly one Annex-B access unit, with the supplied pts.
 * force_idr is 0 or 1; keyframe is 1 only for an IDR picture.
 * On ANY encode error the caller must stop this stream and close: a frame
 * may have been accepted, and retrying could lose or duplicate it. In
 * particular an undersized output buffer is fatal, not a resize/retry API.
 */
int ic_nvenc_encode(IcNvenc *encoder, const uint8_t *nv12, size_t length,
                    int64_t pts, uint32_t force_idr, uint8_t *out,
                    size_t capacity, size_t *written, int64_t *output_pts,
                    uint32_t *keyframe, char *error, size_t error_cap);

/* Bitrate sessions only. Preserves GOP, dimensions and rate-control mode;
 * no reset or forced IDR. Driver failures are terminal, not fallback cues.
 */
int ic_nvenc_set_rate(IcNvenc *encoder, uint32_t kbps, uint32_t fps,
                      char *error, size_t error_cap);

/* NULL-safe. Sends EOS and drains any accepted work before best-effort
 * release. Normally encode already delivered every frame; after an encode
 * error draining is cleanup only, not delivery of a successful packet.
 */
void ic_nvenc_close(IcNvenc *encoder);

#ifdef __cplusplus
}
#endif

#endif
