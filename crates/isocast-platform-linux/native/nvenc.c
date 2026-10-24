/* Synchronous Linux NVENC bridge, using the pinned MIT NVENC 13.0 header.
 * Implementation references (not OBS):
 * https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
 * Sections 3, 4, 5, 6.2 and 8.4: floating CUDA context, external resources,
 * synchronous output, EOS, and reconfiguration.
 * CUDA driver ABI/context flags:
 * https://docs.nvidia.com/cuda/archive/12.8.1/cuda-driver-api/group__CUDA__CTX.html
 * https://docs.nvidia.com/cuda/archive/12.8.1/cuda-driver-api/group__CUDA__TYPES.html
 */
#include "nvenc.h"
#include "nvEncodeAPI.h"

#include <dlfcn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Narrow Linux CUDA driver ABI; load explicit v2 symbols, not the newer
 * cuCtxCreate variants with different arguments. No toolkit headers needed.
 */
typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
#if UINTPTR_MAX == UINT64_MAX
typedef unsigned long long CUdeviceptr;
#else
typedef unsigned int CUdeviceptr;
#endif
enum { CUDA_SUCCESS = 0, CU_CTX_SCHED_BLOCKING_SYNC = 0x04,
       CU_CTX_SYNC_MEMOPS = 0x80 };

struct Cuda {
    CUresult (*init)(unsigned int);
    CUresult (*device_get)(CUdevice *, int);
    CUresult (*ctx_create)(CUcontext *, unsigned int, CUdevice);
    CUresult (*ctx_destroy)(CUcontext);
    CUresult (*ctx_push)(CUcontext);
    CUresult (*ctx_pop)(CUcontext *);
    CUresult (*mem_alloc)(CUdeviceptr *, size_t);
    CUresult (*mem_free)(CUdeviceptr);
    CUresult (*memcpy_htod)(CUdeviceptr, const void *, size_t);
};

struct IcNvenc {
    void *cuda_library;
    void *nvenc_library;
    struct Cuda cuda;
    CUcontext context;
    CUdeviceptr device_input;
    void *session;
    NV_ENCODE_API_FUNCTION_LIST api;
    NV_ENC_INITIALIZE_PARAMS init;
    NV_ENC_CONFIG config;
    NV_ENC_REGISTERED_PTR registered;
    NV_ENC_INPUT_PTR mapped;
    NV_ENC_OUTPUT_PTR bitstream;
    uint8_t *staging;
    size_t staging_size;
    size_t input_size;
    uint32_t pitch;
    uint32_t frame_index;
    uint8_t headers[NV_MAX_SEQ_HDR_LEN];
    uint32_t header_size;
    int initialized;
    int pending;
    int locked;
    int failed;
};

static void clear_error(char *error, size_t capacity)
{
    if (error && capacity)
        error[0] = '\0';
}

static int fail(char *error, size_t capacity, const char *format, ...)
{
    if (error && capacity) {
        va_list args;
        va_start(args, format);
        vsnprintf(error, capacity, format, args);
        va_end(args);
    }
    return -1;
}

static int nv_check(IcNvenc *e, NVENCSTATUS status, const char *function,
                    char *error, size_t capacity)
{
    const char *detail = NULL;
    if (status == NV_ENC_SUCCESS)
        return 0;
    if (e->session && e->api.nvEncGetLastErrorString)
        detail = e->api.nvEncGetLastErrorString(e->session);
    return fail(error, capacity, "%s: NVENC status %d%s%s", function,
                (int)status, detail && *detail ? ": " : "",
                detail && *detail ? detail : "");
}

static int cu_check(CUresult status, const char *function,
                    char *error, size_t capacity)
{
    if (status == CUDA_SUCCESS)
        return 0;
    return fail(error, capacity, "%s: CUDA status %d", function, status);
}

/* dlsym is POSIX; memcpy avoids aliasing a function pointer through void **. */
static int load_symbol(void *library, const char *name, void *out, size_t size,
                       char *error, size_t capacity)
{
    void *symbol;
    const char *detail;
    dlerror();
    symbol = dlsym(library, name);
    detail = dlerror();
    if (detail || !symbol)
        return fail(error, capacity, "dlsym(%s): %s", name,
                    detail ? detail : "missing symbol");
    if (size != sizeof(symbol))
        return fail(error, capacity, "dlsym(%s): unsupported function pointer ABI", name);
    memcpy(out, &symbol, size);
    return 0;
}

static int load_libraries(IcNvenc *e, char *error, size_t error_cap)
{
    NVENCSTATUS (NVENCAPI *max_version)(uint32_t *) = NULL;
    NVENCSTATUS (NVENCAPI *create_instance)(NV_ENCODE_API_FUNCTION_LIST *) = NULL;
    uint32_t version = 0;
    uint32_t required = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;

    e->cuda_library = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!e->cuda_library)
        return fail(error, error_cap, "dlopen(libcuda.so.1): %s", dlerror());
    e->nvenc_library = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!e->nvenc_library)
        return fail(error, error_cap, "dlopen(libnvidia-encode.so.1): %s", dlerror());

#define LOAD(library, name, target) do { \
    if (load_symbol(library, name, &(target), sizeof(target), error, error_cap) < 0) \
        return -1; \
} while (0)
    LOAD(e->cuda_library, "cuInit", e->cuda.init);
    LOAD(e->cuda_library, "cuDeviceGet", e->cuda.device_get);
    LOAD(e->cuda_library, "cuCtxCreate_v2", e->cuda.ctx_create);
    LOAD(e->cuda_library, "cuCtxDestroy_v2", e->cuda.ctx_destroy);
    LOAD(e->cuda_library, "cuCtxPushCurrent_v2", e->cuda.ctx_push);
    LOAD(e->cuda_library, "cuCtxPopCurrent_v2", e->cuda.ctx_pop);
    LOAD(e->cuda_library, "cuMemAlloc_v2", e->cuda.mem_alloc);
    LOAD(e->cuda_library, "cuMemFree_v2", e->cuda.mem_free);
    LOAD(e->cuda_library, "cuMemcpyHtoD_v2", e->cuda.memcpy_htod);
    LOAD(e->nvenc_library, "NvEncodeAPIGetMaxSupportedVersion", max_version);
    LOAD(e->nvenc_library, "NvEncodeAPICreateInstance", create_instance);
#undef LOAD

    if (nv_check(e, max_version(&version), "NvEncodeAPIGetMaxSupportedVersion",
                 error, error_cap) < 0)
        return -1;
    if (version < required)
        return fail(error, error_cap,
                    "NVENC API: driver supports %u.%u; need %u.%u or newer",
                    version >> 4, version & 15, NVENCAPI_MAJOR_VERSION,
                    NVENCAPI_MINOR_VERSION);
    e->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (nv_check(e, create_instance(&e->api), "NvEncodeAPICreateInstance",
                 error, error_cap) < 0)
        return -1;

#define REQUIRE(name) do { \
    if (!e->api.name) \
        return fail(error, error_cap, "NvEncodeAPICreateInstance: missing " #name); \
} while (0)
    REQUIRE(nvEncOpenEncodeSessionEx);
    REQUIRE(nvEncGetEncodeGUIDCount);
    REQUIRE(nvEncGetEncodeGUIDs);
    REQUIRE(nvEncGetEncodeProfileGUIDCount);
    REQUIRE(nvEncGetEncodeProfileGUIDs);
    REQUIRE(nvEncGetEncodePresetCount);
    REQUIRE(nvEncGetEncodePresetGUIDs);
    REQUIRE(nvEncGetInputFormatCount);
    REQUIRE(nvEncGetInputFormats);
    REQUIRE(nvEncGetEncodeCaps);
    REQUIRE(nvEncGetEncodePresetConfigEx);
    REQUIRE(nvEncInitializeEncoder);
    REQUIRE(nvEncGetSequenceParams);
    REQUIRE(nvEncCreateBitstreamBuffer);
    REQUIRE(nvEncDestroyBitstreamBuffer);
    REQUIRE(nvEncRegisterResource);
    REQUIRE(nvEncUnregisterResource);
    REQUIRE(nvEncMapInputResource);
    REQUIRE(nvEncUnmapInputResource);
    REQUIRE(nvEncEncodePicture);
    REQUIRE(nvEncLockBitstream);
    REQUIRE(nvEncUnlockBitstream);
    REQUIRE(nvEncReconfigureEncoder);
    REQUIRE(nvEncDestroyEncoder);
#undef REQUIRE
    return 0;
}

static int enter(IcNvenc *e, char *error, size_t error_cap)
{
    if (!e)
        return fail(error, error_cap, "encoder: must not be NULL");
    if (e->failed)
        return fail(error, error_cap, "encoder: failed; close this stream, do not retry frames");
    if (cu_check(e->cuda.ctx_push(e->context), "cuCtxPushCurrent_v2",
                 error, error_cap) < 0) {
        e->failed = 1;
        return -1;
    }
    return 0;
}

static int leave(IcNvenc *e, int result, char *error, size_t error_cap)
{
    CUcontext popped = NULL;
    CUresult status = e->cuda.ctx_pop(&popped);
    if (status != CUDA_SUCCESS || popped != e->context) {
        e->failed = 1;
        if (result < 0)
            return result; /* Preserve the original failure. */
        if (status != CUDA_SUCCESS)
            return cu_check(status, "cuCtxPopCurrent_v2", error, error_cap);
        return fail(error, error_cap, "cuCtxPopCurrent_v2: unexpected CUDA context");
    }
    return result;
}

static int has_guid(const GUID *guids, uint32_t count, const GUID *wanted)
{
    uint32_t i;
    for (i = 0; i < count; ++i) {
        if (memcmp(&guids[i], wanted, sizeof(*wanted)) == 0)
            return 1;
    }
    return 0;
}

static int get_cap(IcNvenc *e, NV_ENC_CAPS capability, int *value,
                   char *error, size_t error_cap)
{
    NV_ENC_CAPS_PARAM params = {0};
    params.version = NV_ENC_CAPS_PARAM_VER;
    params.capsToQuery = capability;
    return nv_check(e, e->api.nvEncGetEncodeCaps(e->session,
                    NV_ENC_CODEC_H264_GUID, &params, value),
                    "nvEncGetEncodeCaps", error, error_cap);
}

/* Fixed query storage: an unexpectedly large driver list is an explicit
 * creation failure, never an allocation sized by an unchecked count.
 */
static int check_support(IcNvenc *e, uint32_t width, uint32_t height,
                         char *error, size_t error_cap)
{
    GUID guids[64];
    NV_ENC_BUFFER_FORMAT formats[64];
    uint32_t count = 0, actual = 0, i;
    int limit = 0;

#define QUERY(call, name) do { \
    if (nv_check(e, (call), name, error, error_cap) < 0) \
        return -1; \
} while (0)
#define COUNT_OK(n, name) do { \
    if (!(n) || (n) > 64) \
        return fail(error, error_cap, name ": expected 1..64 entries, got %u", (n)); \
} while (0)
    QUERY(e->api.nvEncGetEncodeGUIDCount(e->session, &count), "nvEncGetEncodeGUIDCount");
    COUNT_OK(count, "nvEncGetEncodeGUIDCount");
    QUERY(e->api.nvEncGetEncodeGUIDs(e->session, guids, count, &actual), "nvEncGetEncodeGUIDs");
    if (actual > count || !has_guid(guids, actual, &NV_ENC_CODEC_H264_GUID))
        return fail(error, error_cap, "NVENC device 0: H.264 encoding is not supported");

    QUERY(e->api.nvEncGetEncodeProfileGUIDCount(e->session, NV_ENC_CODEC_H264_GUID, &count),
          "nvEncGetEncodeProfileGUIDCount");
    COUNT_OK(count, "nvEncGetEncodeProfileGUIDCount");
    QUERY(e->api.nvEncGetEncodeProfileGUIDs(e->session, NV_ENC_CODEC_H264_GUID,
                                          guids, count, &actual), "nvEncGetEncodeProfileGUIDs");
    if (actual > count || !has_guid(guids, actual, &NV_ENC_H264_PROFILE_HIGH_GUID))
        return fail(error, error_cap, "NVENC device 0: H.264 High profile is not supported");

    QUERY(e->api.nvEncGetEncodePresetCount(e->session, NV_ENC_CODEC_H264_GUID, &count),
          "nvEncGetEncodePresetCount");
    COUNT_OK(count, "nvEncGetEncodePresetCount");
    QUERY(e->api.nvEncGetEncodePresetGUIDs(e->session, NV_ENC_CODEC_H264_GUID,
                                         guids, count, &actual), "nvEncGetEncodePresetGUIDs");
    if (actual > count || !has_guid(guids, actual, &NV_ENC_PRESET_P4_GUID))
        return fail(error, error_cap, "NVENC device 0: P4 preset is not supported");

    QUERY(e->api.nvEncGetInputFormatCount(e->session, NV_ENC_CODEC_H264_GUID, &count),
          "nvEncGetInputFormatCount");
    COUNT_OK(count, "nvEncGetInputFormatCount");
    QUERY(e->api.nvEncGetInputFormats(e->session, NV_ENC_CODEC_H264_GUID,
                                     formats, count, &actual), "nvEncGetInputFormats");
    if (actual > count)
        return fail(error, error_cap, "nvEncGetInputFormats: count exceeds query capacity");
    for (i = 0; i < actual; ++i) {
        if (formats[i] == NV_ENC_BUFFER_FORMAT_NV12)
            break;
    }
    if (i == actual)
        return fail(error, error_cap, "NVENC device 0: NV12 input is not supported");
#undef COUNT_OK
#undef QUERY

    if (get_cap(e, NV_ENC_CAPS_WIDTH_MAX, &limit, error, error_cap) < 0)
        return -1;
    if (limit <= 0 || width > (uint32_t)limit)
        return fail(error, error_cap, "width: %u exceeds NVENC device 0 maximum %d", width, limit);
    if (get_cap(e, NV_ENC_CAPS_HEIGHT_MAX, &limit, error, error_cap) < 0)
        return -1;
    if (limit <= 0 || height > (uint32_t)limit)
        return fail(error, error_cap, "height: %u exceeds NVENC device 0 maximum %d", height, limit);
    return 0;
}

static int fetch_headers(IcNvenc *e, char *error, size_t error_cap)
{
    NV_ENC_SEQUENCE_PARAM_PAYLOAD params = {0};
    e->header_size = 0;
    params.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
    params.inBufferSize = sizeof(e->headers);
    params.spsppsBuffer = e->headers;
    params.outSPSPPSPayloadSize = &e->header_size;
    if (nv_check(e, e->api.nvEncGetSequenceParams(e->session, &params),
                 "nvEncGetSequenceParams", error, error_cap) < 0)
        return -1;
    if (!e->header_size || e->header_size > sizeof(e->headers))
        return fail(error, error_cap, "nvEncGetSequenceParams: invalid SPS/PPS size %u",
                    e->header_size);
    return 0;
}

static void set_bitrate(NV_ENC_CONFIG *config, uint32_t kbps)
{
    config->rcParams.averageBitRate = kbps * 1000;
    config->rcParams.maxBitRate = kbps * 1000;
    /* Hold the target over one second without starving periodic IDRs. */
    config->rcParams.vbvBufferSize = kbps * 1000;
    config->rcParams.vbvInitialDelay = config->rcParams.vbvBufferSize;
}

static int initialize(IcNvenc *e, uint32_t width, uint32_t height, uint32_t fps,
                      uint32_t kbps, uint32_t gop, uint32_t qp, uint32_t cbr,
                      char *error, size_t error_cap)
{
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {0};
    NV_ENC_PRESET_CONFIG preset = {0};
    NV_ENC_REGISTER_RESOURCE resource = {0};
    NV_ENC_CREATE_BITSTREAM_BUFFER output = {0};
    NV_ENC_CONFIG_H264 *h264;
    NV_ENC_CONFIG_H264_VUI_PARAMETERS *vui;
    int modes = 0;

    session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    session.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    session.device = e->context;
    session.apiVersion = NVENCAPI_VERSION;
    if (nv_check(e, e->api.nvEncOpenEncodeSessionEx(&session, &e->session),
                 "nvEncOpenEncodeSessionEx", error, error_cap) < 0)
        return -1;
    if (check_support(e, width, height, error, error_cap) < 0)
        return -1;
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (nv_check(e, e->api.nvEncGetEncodePresetConfigEx(e->session,
                 NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID,
                 NV_ENC_TUNING_INFO_LOW_LATENCY, &preset),
                 "nvEncGetEncodePresetConfigEx", error, error_cap) < 0)
        return -1;

    e->config = preset.presetCfg;
    e->config.version = NV_ENC_CONFIG_VER;
    e->config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
    e->config.gopLength = gop;
    e->config.frameIntervalP = 1;
    e->config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    /* Keep codec preset tuning, but explicitly own every RC option so a
     * preset cannot introduce buffering, AQ, multipass or a different QP.
     */
    memset(&e->config.rcParams, 0, sizeof(e->config.rcParams));
    e->config.rcParams.version = NV_ENC_RC_PARAMS_VER;
    e->config.rcParams.zeroReorderDelay = 1;
    if (qp == UINT32_MAX) {
        e->config.rcParams.rateControlMode = cbr ? NV_ENC_PARAMS_RC_CBR : NV_ENC_PARAMS_RC_VBR;
        if (get_cap(e, NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES, &modes, error, error_cap) < 0)
            return -1;
        if (!(modes & e->config.rcParams.rateControlMode))
            return fail(error, error_cap, "NVENC device 0: %s rate control is not supported",
                        cbr ? "CBR" : "VBR");
        set_bitrate(&e->config, kbps);
    } else {
        e->config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        e->config.rcParams.constQP.qpInterP = qp;
        e->config.rcParams.constQP.qpInterB = qp;
        e->config.rcParams.constQP.qpIntra = qp;
    }
    h264 = &e->config.encodeCodecConfig.h264Config;
    h264->level = NV_ENC_LEVEL_AUTOSELECT;
    h264->idrPeriod = gop;
    h264->chromaFormatIDC = 1;
    h264->inputBitDepth = NV_ENC_BIT_DEPTH_8;
    h264->outputBitDepth = NV_ENC_BIT_DEPTH_8;
    h264->enableTemporalSVC = 0;
    h264->enableStereoMVC = 0;
    h264->hierarchicalPFrames = 0;
    h264->hierarchicalBFrames = 0;
    h264->enableIntraRefresh = 0;
    h264->enableLTR = 0;
    h264->useBFramesAsRef = NV_ENC_BFRAME_REF_MODE_DISABLED;
    h264->tfLevel = NV_ENC_TEMPORAL_FILTER_LEVEL_0;
    h264->numTemporalLayers = 1;
    h264->maxTemporalLayers = 1;
    h264->disableSPSPPS = 0;
    h264->repeatSPSPPS = 1;
    h264->enableFillerDataInsertion = qp == UINT32_MAX && cbr;
    h264->qpPrimeYZeroTransformBypassFlag = 0;
    vui = &h264->h264VUIParameters;
    memset(vui, 0, sizeof(*vui));
    vui->videoSignalTypePresentFlag = 1;
    vui->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui->videoFullRangeFlag = 0;
    vui->colourDescriptionPresentFlag = 1;
    vui->colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
    vui->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
    vui->colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
    vui->bitstreamRestrictionFlag = 1;

    e->init.version = NV_ENC_INITIALIZE_PARAMS_VER;
    e->init.encodeGUID = NV_ENC_CODEC_H264_GUID;
    e->init.presetGUID = NV_ENC_PRESET_P4_GUID;
    e->init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
    e->init.encodeWidth = width;
    e->init.encodeHeight = height;
    e->init.darWidth = width;
    e->init.darHeight = height;
    e->init.frameRateNum = fps;
    e->init.frameRateDen = 1;
    e->init.enablePTD = 1;
    e->init.enableEncodeAsync = 0;
    e->init.encodeConfig = &e->config;
    if (nv_check(e, e->api.nvEncInitializeEncoder(e->session, &e->init),
                 "nvEncInitializeEncoder", error, error_cap) < 0)
        return -1;
    e->initialized = 1;
    /* Fail here, before Rust can reserve a recording file. */
    if (fetch_headers(e, error, error_cap) < 0)
        return -1;

    e->pitch = (width + 255u) & ~255u;
    e->input_size = (size_t)width * height * 3 / 2;
    e->staging_size = (size_t)e->pitch * height * 3 / 2;
    e->staging = calloc(1, e->staging_size);
    if (!e->staging)
        return fail(error, error_cap, "NV12 staging: cannot allocate %zu bytes", e->staging_size);
    if (cu_check(e->cuda.mem_alloc(&e->device_input, e->staging_size),
                 "cuMemAlloc_v2", error, error_cap) < 0)
        return -1;
    resource.version = NV_ENC_REGISTER_RESOURCE_VER;
    resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    resource.resourceToRegister = (void *)(uintptr_t)e->device_input;
    resource.width = width;
    resource.height = height;
    resource.pitch = e->pitch;
    resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    resource.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (nv_check(e, e->api.nvEncRegisterResource(e->session, &resource),
                 "nvEncRegisterResource", error, error_cap) < 0)
        return -1;
    e->registered = resource.registeredResource;
    output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    if (nv_check(e, e->api.nvEncCreateBitstreamBuffer(e->session, &output),
                 "nvEncCreateBitstreamBuffer", error, error_cap) < 0)
        return -1;
    e->bitstream = output.bitstreamBuffer;
    return 0;
}

IcNvenc *ic_nvenc_open(uint32_t width, uint32_t height, uint32_t fps,
                      uint32_t kbps, uint32_t gop, uint32_t qp, uint32_t cbr,
                      char *error, size_t error_cap)
{
    IcNvenc *e;
    CUdevice device;
    int result;
    clear_error(error, error_cap);
    if (width < 16 || height < 16 || (width & 1) || (height & 1) ||
        !((width <= 3840 && height <= 2160) || (width <= 2160 && height <= 3840))) {
        fail(error, error_cap, "dimensions: even, >=16, at most 3840x2160 or 2160x3840 required");
        return NULL;
    }
    if (fps < 1 || fps > 60 || !gop || cbr > 1 || (qp != UINT32_MAX && qp > 51)) {
        fail(error, error_cap, "settings: fps=1..60, gop>=1, cbr=0|1, qp=0..51|UINT32_MAX required");
        return NULL;
    }
    if (qp == UINT32_MAX && (!kbps || kbps > UINT32_MAX / 1000)) {
        fail(error, error_cap, "kbps: bitrate mode requires 1..%u", UINT32_MAX / 1000);
        return NULL;
    }
    e = calloc(1, sizeof(*e));
    if (!e) {
        fail(error, error_cap, "encoder: allocation failed");
        return NULL;
    }
    if (load_libraries(e, error, error_cap) < 0 ||
        cu_check(e->cuda.init(0), "cuInit", error, error_cap) < 0 ||
        cu_check(e->cuda.device_get(&device, 0), "cuDeviceGet(0)", error, error_cap) < 0)
        goto failed;
    /* Device 0 may differ from Vulkan's adapter. CPU-copy input makes that
     * safe, but this is not GPU affinity selection or a zero-copy path.
     * SYNC_MEMOPS makes the pageable HtoD upload complete before NVENC use.
     */
    if (cu_check(e->cuda.ctx_create(&e->context,
                 CU_CTX_SCHED_BLOCKING_SYNC | CU_CTX_SYNC_MEMOPS, device),
                 "cuCtxCreate_v2", error, error_cap) < 0)
        goto failed;
    /* Creation pushes the context. Float it immediately, then use exactly
     * the same push/pop discipline as every subsequent public operation.
     */
    if (leave(e, 0, error, error_cap) < 0)
        goto failed;
    if (enter(e, error, error_cap) < 0)
        goto failed;
    result = initialize(e, width, height, fps, kbps, gop, qp, cbr, error, error_cap);
    if (leave(e, result, error, error_cap) < 0)
        goto failed;
    return e;

failed:
    ic_nvenc_close(e);
    return NULL;
}

int ic_nvenc_headers(IcNvenc *e, uint8_t *out, size_t capacity,
                     size_t *written, char *error, size_t error_cap)
{
    int result = 0;
    clear_error(error, error_cap);
    if (written)
        *written = 0;
    if (!written || (!out && capacity))
        return fail(error, error_cap, "headers: written and a valid output buffer required");
    if (enter(e, error, error_cap) < 0)
        return -1;
    *written = e->header_size;
    if (capacity < e->header_size)
        result = fail(error, error_cap, "headers.capacity: need %u bytes, got %zu",
                      e->header_size, capacity);
    else
        memcpy(out, e->headers, e->header_size);
    return leave(e, result, error, error_cap);
}

int ic_nvenc_encode(IcNvenc *e, const uint8_t *nv12, size_t length,
                    int64_t pts, uint32_t force_idr, uint8_t *out,
                    size_t capacity, size_t *written, int64_t *output_pts,
                    uint32_t *keyframe, char *error, size_t error_cap)
{
    NV_ENC_MAP_INPUT_RESOURCE map = {0};
    NV_ENC_PIC_PARAMS pic = {0};
    NV_ENC_LOCK_BITSTREAM lock = {0};
    NVENCSTATUS status;
    uint32_t row;
    int result = -1;

    clear_error(error, error_cap);
    if (written)
        *written = 0;
    if (output_pts)
        *output_pts = 0;
    if (keyframe)
        *keyframe = 0;
    if (!e || !nv12 || !out || !capacity || !written || !output_pts || !keyframe)
        return fail(error, error_cap, "encode: encoder, input and output buffers/pointers required");
    if (length != e->input_size || force_idr > 1)
        return fail(error, error_cap, "encode: NV12 length must be %zu (got %zu), force_idr=0|1",
                    e->input_size, length);
    if (enter(e, error, error_cap) < 0)
        return -1;
    for (row = 0; row < e->init.encodeHeight * 3 / 2; ++row)
        memcpy(e->staging + (size_t)row * e->pitch,
               nv12 + (size_t)row * e->init.encodeWidth, e->init.encodeWidth);
    if (cu_check(e->cuda.memcpy_htod(e->device_input, e->staging, e->staging_size),
                 "cuMemcpyHtoD_v2", error, error_cap) < 0)
        goto done;
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = e->registered;
    if (nv_check(e, e->api.nvEncMapInputResource(e->session, &map),
                 "nvEncMapInputResource", error, error_cap) < 0)
        goto done;
    e->mapped = map.mappedResource;
    if (map.mappedBufferFmt != NV_ENC_BUFFER_FORMAT_NV12) {
        fail(error, error_cap, "nvEncMapInputResource: expected NV12, got format %u",
             (unsigned int)map.mappedBufferFmt);
        goto done;
    }
    pic.version = NV_ENC_PIC_PARAMS_VER;
    pic.inputWidth = e->init.encodeWidth;
    pic.inputHeight = e->init.encodeHeight;
    pic.inputPitch = e->pitch;
    pic.frameIdx = e->frame_index;
    pic.inputTimeStamp = (uint64_t)pts;
    pic.inputBuffer = e->mapped;
    pic.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pic.outputBitstream = e->bitstream;
    if (force_idr)
        pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    status = e->api.nvEncEncodePicture(e->session, &pic);
    /* NEED_MORE_INPUT accepts input but violates this one-frame contract.
     * Preserve its mapped input/output until EOS + drain in close. Never
     * return an empty successful packet or reuse an in-flight resource.
     */
    if (status == NV_ENC_SUCCESS || status == NV_ENC_ERR_NEED_MORE_INPUT)
        e->pending = 1;
    if (nv_check(e, status, "nvEncEncodePicture (synchronous, no frame buffering)",
                 error, error_cap) < 0)
        goto done;
    lock.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock.outputBitstream = e->bitstream;
    lock.doNotWait = 0; /* Blocking native wait; no timeout guarantee. */
    if (nv_check(e, e->api.nvEncLockBitstream(e->session, &lock),
                 "nvEncLockBitstream", error, error_cap) < 0)
        goto done;
    e->locked = 1;
    /* hwEncodeStatus is not NVENCSTATUS (it reports sub-frame progress).
     * Full-frame synchronous LockBitstream success is the completion signal.
     */
    if (lock.outputTimeStamp != (uint64_t)pts ||
        (lock.pictureType != NV_ENC_PIC_TYPE_IDR && lock.pictureType != NV_ENC_PIC_TYPE_I &&
         lock.pictureType != NV_ENC_PIC_TYPE_P)) {
        fail(error, error_cap, "nvEncLockBitstream: reordered timestamp or unexpected picture type %u",
             (unsigned int)lock.pictureType);
        goto done;
    }
    if (force_idr && lock.pictureType != NV_ENC_PIC_TYPE_IDR) {
        fail(error, error_cap, "nvEncLockBitstream: requested IDR was not produced");
        goto done;
    }
    if (!lock.bitstreamBufferPtr || !lock.bitstreamSizeInBytes) {
        fail(error, error_cap, "nvEncLockBitstream: empty encoded picture");
        goto done;
    }
    if (lock.bitstreamSizeInBytes > capacity) {
        fail(error, error_cap, "encode.capacity: need %u bytes, got %zu; frame accepted, close stream",
             lock.bitstreamSizeInBytes, capacity);
        goto done;
    }
    memcpy(out, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
    result = 0;

done:
    if (e->locked) {
        status = e->api.nvEncUnlockBitstream(e->session, e->bitstream);
        if (status == NV_ENC_SUCCESS) {
            e->locked = 0;
            e->pending = 0;
        } else if (result == 0) {
            result = nv_check(e, status, "nvEncUnlockBitstream", error, error_cap);
        }
    }
    if (e->mapped && !e->pending) {
        status = e->api.nvEncUnmapInputResource(e->session, e->mapped);
        if (status == NV_ENC_SUCCESS)
            e->mapped = NULL;
        else if (result == 0)
            result = nv_check(e, status, "nvEncUnmapInputResource", error, error_cap);
    }
    if (result < 0)
        e->failed = 1;
    result = leave(e, result, error, error_cap);
    if (result == 0) {
        *written = lock.bitstreamSizeInBytes;
        *output_pts = pts;
        *keyframe = lock.pictureType == NV_ENC_PIC_TYPE_IDR;
        ++e->frame_index;
    }
    return result;
}

int ic_nvenc_set_rate(IcNvenc *e, uint32_t kbps, uint32_t fps,
                      char *error, size_t error_cap)
{
    NV_ENC_RECONFIGURE_PARAMS params = {0};
    NV_ENC_CONFIG config;
    int supported = 0, result;
    clear_error(error, error_cap);
    if (!e)
        return fail(error, error_cap, "encoder: must not be NULL");
    if (!kbps || kbps > UINT32_MAX / 1000 || fps < 1 || fps > 60)
        return fail(error, error_cap, "rate: kbps=1..%u and fps=1..60 required", UINT32_MAX / 1000);
    if (e->config.rcParams.rateControlMode == NV_ENC_PARAMS_RC_CONSTQP)
        return fail(error, error_cap, "rate: fixed-QP sessions cannot change bitrate");
    if (enter(e, error, error_cap) < 0)
        return -1;
    result = get_cap(e, NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE, &supported, error, error_cap);
    if (result < 0)
        goto done;
    if (!supported) {
        result = fail(error, error_cap, "NVENC device 0: dynamic bitrate changes are not supported");
        goto done;
    }
    config = e->config;
    set_bitrate(&config, kbps);
    params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    params.reInitEncodeParams = e->init;
    params.reInitEncodeParams.frameRateNum = fps;
    params.reInitEncodeParams.encodeConfig = &config;
    result = nv_check(e, e->api.nvEncReconfigureEncoder(e->session, &params),
                      "nvEncReconfigureEncoder", error, error_cap);
    if (result < 0) {
        e->failed = 1;
        goto done;
    }
    e->config = config;
    e->init.frameRateNum = fps;
    result = fetch_headers(e, error, error_cap);
    if (result < 0)
        e->failed = 1;
done:
    return leave(e, result, error, error_cap);
}

void ic_nvenc_close(IcNvenc *e)
{
    int current = 0;
    if (!e)
        return;
    if (e->context)
        current = e->cuda.ctx_push(e->context) == CUDA_SUCCESS;
    if (e->session) {
        /* Even an error path attempts EOS and drains the one accepted frame.
         * With a lost context/device these calls can fail; close is best
         * effort, and cannot report a packet or promise a bounded wait.
         */
        if (e->initialized) {
            NV_ENC_PIC_PARAMS eos = {0};
            eos.version = NV_ENC_PIC_PARAMS_VER;
            eos.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            e->api.nvEncEncodePicture(e->session, &eos);
        }
        if (e->pending && !e->locked) {
            NV_ENC_LOCK_BITSTREAM lock = {0};
            lock.version = NV_ENC_LOCK_BITSTREAM_VER;
            lock.outputBitstream = e->bitstream;
            lock.doNotWait = 0;
            if (e->api.nvEncLockBitstream(e->session, &lock) == NV_ENC_SUCCESS)
                e->locked = 1;
        }
        if (e->locked)
            e->api.nvEncUnlockBitstream(e->session, e->bitstream);
        if (e->mapped)
            e->api.nvEncUnmapInputResource(e->session, e->mapped);
        if (e->registered)
            e->api.nvEncUnregisterResource(e->session, e->registered);
        if (e->bitstream)
            e->api.nvEncDestroyBitstreamBuffer(e->session, e->bitstream);
        e->api.nvEncDestroyEncoder(e->session);
    }
    if (current) {
        CUcontext popped = NULL;
        if (e->device_input)
            e->cuda.mem_free(e->device_input);
        e->cuda.ctx_pop(&popped);
    }
    /* Destroy also releases allocations if a lost context prevented free. */
    if (e->context)
        e->cuda.ctx_destroy(e->context);
    free(e->staging);
    if (e->nvenc_library)
        dlclose(e->nvenc_library);
    if (e->cuda_library)
        dlclose(e->cuda_library);
    free(e);
}
