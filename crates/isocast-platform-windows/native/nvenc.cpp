// Isocast's bounded D3D11/NVENC ABI boundary. No pixels cross this boundary.
// Public NVIDIA header: nv-codec-headers n13.0.19.0 (trailing blank removed).
// Upstream SHA-256:
// 4fe4094541ef0f8a13249d97a8692dc5f835a6e9dd42eeadb3e2f7321d54dc7e.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <cstdint>
#include <atomic>
#include <cstdio>
#include <new>
#include <d3d11.h>
#include "nvEncodeAPI.h"

namespace {
// Asynchronous CPU bitstream output needs at least 4 + B-frame count slots.
// This encoder disables B-frames/lookahead; keep the matching Rust bound.
constexpr unsigned slots_count = 4;
constexpr int again = -1;
constexpr int internal_error = -2;
std::atomic<uint32_t> sessions{0};

enum InputFormat : uint32_t { bgra8 = 0, rgba10 = 1, nv12 = 2 };

struct Config {
    uint32_t width, height, fps, bitrate, constant_bitrate, keyframe_interval;
    uint32_t codec, input_depth, output_depth, quality_qp, input_format, preset;
};
static_assert(sizeof(Config) == 48);

struct Packet {
    const uint8_t *data;
    uint64_t pts;
    uint32_t size, keyframe;
};
static_assert(sizeof(Packet) == 24);

struct Resource {
    ID3D11Texture2D *texture = nullptr;
    NV_ENC_REGISTERED_PTR registered = nullptr;
    NV_ENC_INPUT_PTR mapped = nullptr;
};

struct Slot {
    NV_ENC_OUTPUT_PTR buffer = nullptr;
    HANDLE event = nullptr;
    ID3D11Query *copy = nullptr;
    bool copying = false;
    bool registered = false, completed = false, locked = false;
    Resource input;
};

struct Encoder {
    HMODULE library = nullptr;
    void *session = nullptr;
    NV_ENCODE_API_FUNCTION_LIST api = {};
    NV_ENC_INITIALIZE_PARAMS init = {};
    NV_ENC_CONFIG config = {};
    NV_ENC_BUFFER_FORMAT input_format = NV_ENC_BUFFER_FORMAT_ARGB;
    DXGI_FORMAT texture_format = DXGI_FORMAT_B8G8R8A8_UNORM;
    ID3D11DeviceContext *context = nullptr;
    Slot slots[slots_count];
    HANDLE eos = nullptr;
    bool eos_registered = false, submitted = false, flushing = false, flushed = false;
    bool counted = false;
    bool recording = false;
    // Only a proved rejection before encode (or a clean BUSY unmap) may be
    // recovered without quarantining the native session.
    bool rejected = false;
    unsigned head = 0, count = 0;
    char error[512] = {};
    uint8_t headers[4096] = {};
    uint32_t headers_size = 0;

    int fail(const char *operation, NVENCSTATUS status) {
        const char *detail = session && api.nvEncGetLastErrorString
            ? api.nvEncGetLastErrorString(session) : "";
        std::snprintf(error, sizeof(error), "%s: NVENC error %d%s%s", operation,
                      static_cast<int>(status), detail && *detail ? ": " : "",
                      detail ? detail : "");
        return static_cast<int>(status);
    }

    int win_error(const char *operation) {
        std::snprintf(error, sizeof(error), "%s: Windows error %lu", operation, GetLastError());
        return internal_error;
    }

    int unregister(Resource &resource) {
        if (resource.mapped) {
            auto status = api.nvEncUnmapInputResource(session, resource.mapped);
            if (status != NV_ENC_SUCCESS) return fail("unmap input texture", status);
            resource.mapped = nullptr;
        }
        if (resource.registered) {
            auto status = api.nvEncUnregisterResource(session, resource.registered);
            if (status != NV_ENC_SUCCESS) return fail("unregister input texture", status);
            resource.registered = nullptr;
        }
        if (resource.texture) resource.texture->Release();
        resource = {};
        return 0;
    }

    int resource(ID3D11Device *device, Resource &input) {
        D3D11_TEXTURE2D_DESC desc = {};
        desc.Width = init.encodeWidth;
        desc.Height = init.encodeHeight;
        desc.MipLevels = desc.ArraySize = desc.SampleDesc.Count = 1;
        desc.Format = texture_format;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = texture_format == DXGI_FORMAT_NV12
            ? D3D11_BIND_SHADER_RESOURCE : D3D11_BIND_RENDER_TARGET;
        HRESULT result = device->CreateTexture2D(&desc, nullptr, &input.texture);
        if (FAILED(result)) {
            std::snprintf(error, sizeof(error), "allocate NVENC GPU input: HRESULT 0x%08lx", static_cast<unsigned long>(result));
            return internal_error;
        }
        NV_ENC_REGISTER_RESOURCE reg = {};
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.resourceToRegister = input.texture;
        reg.width = init.encodeWidth;
        reg.height = init.encodeHeight;
        reg.bufferFormat = input_format;
        reg.bufferUsage = NV_ENC_INPUT_IMAGE;
        auto native = api.nvEncRegisterResource(session, &reg);
        if (native != NV_ENC_SUCCESS) return fail("register D3D11 input texture", native);
        input.registered = reg.registeredResource;
        return 0;
    }

    int copy_ready(Slot &slot) {
        if (!slot.copying) return 0;
        BOOL complete = FALSE;
        HRESULT result = context->GetData(slot.copy, &complete, sizeof(complete),
                                          D3D11_ASYNC_GETDATA_DONOTFLUSH);
        if (result == S_OK && complete) {
            slot.copying = false;
            return 0;
        }
        if (FAILED(result)) {
            std::snprintf(error, sizeof(error), "complete NVENC GPU input copy: HRESULT 0x%08lx", static_cast<unsigned long>(result));
            return internal_error;
        }
        return again;
    }

    int wait_copy(uint32_t timeout_ms) {
        const auto deadline = GetTickCount64() + timeout_ms;
        // Rejections may also prove preceding accepted copies early. Their
        // source leases still remain in the pending FIFO until encoded output.
        for (auto &slot : slots) {
            while (slot.copying) {
                int result = copy_ready(slot);
                if (result != again) {
                    if (result) return result;
                    break;
                }
                if (GetTickCount64() >= deadline) {
                    std::snprintf(error, sizeof(error), "NVENC GPU input copy did not finish within %u ms", timeout_ms);
                    return internal_error;
                }
                Sleep(1);
            }
        }
        return 0;
    }

    // Only called with no GPU input outstanding (or before any submission).
    // A caller with a stalled GPU must retain this object, not free its inputs.
    int release() {
        // An unaccepted copy is still GPU work, even if mapping or EOS failed.
        int copied = wait_copy(0);
        if (copied) return copied;
        if (session) {
            for (auto &slot : slots) {
                int status = unregister(slot.input);
                if (status) return status;
            }
            for (auto &slot : slots) {
                if (slot.locked) {
                    auto status = api.nvEncUnlockBitstream(session, slot.buffer);
                    if (status != NV_ENC_SUCCESS) return fail("unlock output during teardown", status);
                    slot.locked = false;
                }
                if (slot.registered) {
                    NV_ENC_EVENT_PARAMS event = {};
                    event.version = NV_ENC_EVENT_PARAMS_VER;
                    event.completionEvent = slot.event;
                    auto status = api.nvEncUnregisterAsyncEvent(session, &event);
                    if (status != NV_ENC_SUCCESS) return fail("unregister completion event", status);
                    slot.registered = false;
                }
                if (slot.buffer) {
                    auto status = api.nvEncDestroyBitstreamBuffer(session, slot.buffer);
                    if (status != NV_ENC_SUCCESS) return fail("destroy encoded output buffer", status);
                    slot.buffer = nullptr;
                }
            }
            if (eos_registered) {
                NV_ENC_EVENT_PARAMS event = {};
                event.version = NV_ENC_EVENT_PARAMS_VER;
                event.completionEvent = eos;
                auto status = api.nvEncUnregisterAsyncEvent(session, &event);
                if (status != NV_ENC_SUCCESS) return fail("unregister flush event", status);
                eos_registered = false;
            }
            auto status = api.nvEncDestroyEncoder(session);
            if (status != NV_ENC_SUCCESS) return fail("destroy encoder", status);
            session = nullptr;
            if (counted) { --sessions; counted = false; }
        }
        for (auto &slot : slots) {
            if (slot.copy) slot.copy->Release();
            if (slot.event) CloseHandle(slot.event);
        }
        if (eos) CloseHandle(eos);
        if (context) context->Release();
        if (library) FreeLibrary(library);
        return 0;
    }

    int event(HANDLE *handle) {
        *handle = CreateEventW(nullptr, FALSE, FALSE, nullptr);
        if (!*handle) return win_error("create NVENC completion event");
        NV_ENC_EVENT_PARAMS params = {};
        params.version = NV_ENC_EVENT_PARAMS_VER;
        params.completionEvent = *handle;
        auto status = api.nvEncRegisterAsyncEvent(session, &params);
        return status == NV_ENC_SUCCESS ? 0 : fail("register NVENC completion event", status);
    }

    int open(void *device, const Config &spec) {
        recording = spec.quality_qp != UINT32_MAX;
        if (recording && spec.quality_qp > 51) {
            std::snprintf(error, sizeof(error), "video.quantizer: NVENC valid range is 0..=51");
            return internal_error;
        }
        if (spec.codec > 1 || (spec.output_depth != 8 && spec.output_depth != 10)
            || (spec.input_depth != 8 && spec.input_depth != 10)
            || spec.input_format > nv12
            || spec.input_depth != (spec.input_format == rgba10 ? 10u : 8u)
            || spec.input_depth > spec.output_depth
            || (spec.codec == 0 && spec.output_depth != 8)) {
            std::snprintf(error, sizeof(error), "unsupported codec/input format/input/output bit-depth combination");
            return internal_error;
        }
        const auto codec = spec.codec == 0 ? NV_ENC_CODEC_H264_GUID : NV_ENC_CODEC_HEVC_GUID;
        if (spec.input_format == rgba10) {
            input_format = NV_ENC_BUFFER_FORMAT_ABGR10;
            texture_format = DXGI_FORMAT_R10G10B10A2_UNORM;
        } else if (spec.input_format == nv12) {
            input_format = NV_ENC_BUFFER_FORMAT_NV12;
            texture_format = DXGI_FORMAT_NV12;
        }
        library = LoadLibraryExW(L"nvEncodeAPI64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
        if (!library) return win_error("load the system NVIDIA encoder driver");
        using Version = NVENCSTATUS (NVENCAPI *)(uint32_t *);
        using Create = NVENCSTATUS (NVENCAPI *)(NV_ENCODE_API_FUNCTION_LIST *);
        auto version = reinterpret_cast<Version>(GetProcAddress(library, "NvEncodeAPIGetMaxSupportedVersion"));
        auto create = reinterpret_cast<Create>(GetProcAddress(library, "NvEncodeAPICreateInstance"));
        if (!version || !create) return win_error("find NVIDIA encoder entry points");
        uint32_t supported = 0;
        auto status = version(&supported);
        if (status != NV_ENC_SUCCESS) return fail("query NVIDIA encoder API", status);
        if (supported < (NVENCAPI_MAJOR_VERSION << 4)) {
            std::snprintf(error, sizeof(error), "NVIDIA driver API %u.%u is older than required 13.0", supported >> 4, supported & 15);
            return internal_error;
        }
        api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        status = create(&api);
        if (status != NV_ENC_SUCCESS) return fail("load NVIDIA encoder API", status);
        NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open = {};
        open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        open.device = device;
        open.apiVersion = NVENCAPI_VERSION;
        status = api.nvEncOpenEncodeSessionEx(&open, &session);
        if (status != NV_ENC_SUCCESS) return fail("open NVIDIA encoder session (check driver session capacity and GPU memory)", status);
        ++sessions;
        counted = true;
        NV_ENC_CAPS_PARAM cap = {};
        cap.version = NV_ENC_CAPS_PARAM_VER;
        cap.capsToQuery = NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT;
        int asynchronous = 0;
        status = api.nvEncGetEncodeCaps(session, codec, &cap, &asynchronous);
        if (status != NV_ENC_SUCCESS) return fail("query codec asynchronous support", status);
        if (!asynchronous) {
            std::snprintf(error, sizeof(error), "NVIDIA asynchronous encoding is unavailable (requires a WDDM driver)");
            return internal_error;
        }
        if (spec.output_depth == 10) {
            cap.capsToQuery = NV_ENC_CAPS_SUPPORT_10BIT_ENCODE;
            int ten_bit = 0;
            status = api.nvEncGetEncodeCaps(session, codec, &cap, &ten_bit);
            if (status != NV_ENC_SUCCESS) return fail("query ten-bit encode support", status);
            if (!ten_bit) {
                std::snprintf(error, sizeof(error), "HEVC Main10 is not supported by this GPU");
                return internal_error;
            }
        }
        NV_ENC_BUFFER_FORMAT formats[64] = {};
        uint32_t format_count = 0;
        status = api.nvEncGetInputFormatCount(session, codec, &format_count);
        if (status != NV_ENC_SUCCESS) return fail("query codec input format count", status);
        if (format_count > 64) {
            std::snprintf(error, sizeof(error), "NVENC returned too many input formats");
            return internal_error;
        }
        status = api.nvEncGetInputFormats(session, codec, formats, 64, &format_count);
        if (status != NV_ENC_SUCCESS) return fail("query codec input formats", status);
        bool found = false;
        for (uint32_t i = 0; i < format_count; ++i) found |= formats[i] == input_format;
        if (!found) {
            std::snprintf(error, sizeof(error), "requested input format is not supported for this codec");
            return internal_error;
        }
        NV_ENC_PRESET_CONFIG preset = {};
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        const GUID presets[] = {NV_ENC_PRESET_P1_GUID, NV_ENC_PRESET_P2_GUID,
            NV_ENC_PRESET_P3_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_PRESET_P5_GUID,
            NV_ENC_PRESET_P6_GUID, NV_ENC_PRESET_P7_GUID};
        if (spec.preset < 1 || spec.preset > 7) {
            std::snprintf(error, sizeof(error), "NVENC preset must be P1..=P7");
            return internal_error;
        }
        const GUID &preset_guid = presets[spec.preset - 1];
        status = api.nvEncGetEncodePresetConfigEx(session, codec,
            preset_guid, NV_ENC_TUNING_INFO_LOW_LATENCY, &preset);
        if (status != NV_ENC_SUCCESS) return fail("query low-latency preset", status);
        config = preset.presetCfg;
        config.profileGUID = spec.codec == 0 ? NV_ENC_H264_PROFILE_HIGH_GUID
            : spec.output_depth == 10 ? NV_ENC_HEVC_PROFILE_MAIN10_GUID : NV_ENC_HEVC_PROFILE_MAIN_GUID;
        config.gopLength = spec.keyframe_interval;
        config.frameIntervalP = 1;
        config.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
        auto &rc = config.rcParams;
        if (recording) {
            // CONSTQP is zero, not a testable bit in SUPPORTED_RATECONTROL_MODES.
            // InitializeEncoder validates this mode together with spatial AQ8;
            // never silently disable AQ or fall back to bitrate control.
            rc.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
            rc.constQP.qpIntra = rc.constQP.qpInterP = rc.constQP.qpInterB = spec.quality_qp;
            rc.averageBitRate = rc.maxBitRate = rc.vbvBufferSize = rc.vbvInitialDelay = 0;
            rc.targetQuality = rc.targetQualityLSB = 0;
            rc.enableMinQP = rc.enableMaxQP = rc.enableInitialRCQP = 0;
        } else {
            rc.rateControlMode = spec.constant_bitrate ? NV_ENC_PARAMS_RC_CBR : NV_ENC_PARAMS_RC_VBR;
            rc.averageBitRate = rc.maxBitRate = rc.vbvBufferSize = spec.bitrate;
        }
        rc.enableLookahead = 0;
        rc.lookaheadDepth = 0;
        rc.enableAQ = 1;
        rc.aqStrength = 8;
        rc.enableTemporalAQ = 0;
        rc.zeroReorderDelay = 1;
        rc.multiPass = NV_ENC_MULTI_PASS_DISABLED;
        NV_ENC_CONFIG_H264_VUI_PARAMETERS *colour;
        if (spec.codec == 0) {
            auto &h264 = config.encodeCodecConfig.h264Config;
            h264.idrPeriod = spec.keyframe_interval;
            h264.repeatSPSPPS = 1;
            h264.outputAUD = 1;
            h264.enableFillerDataInsertion = !recording && spec.constant_bitrate != 0;
            h264.chromaFormatIDC = 1;
            h264.inputBitDepth = h264.outputBitDepth = NV_ENC_BIT_DEPTH_8;
            colour = &h264.h264VUIParameters;
        } else {
            auto &hevc = config.encodeCodecConfig.hevcConfig;
            hevc.idrPeriod = spec.keyframe_interval;
            hevc.repeatSPSPPS = 1;
            hevc.outputAUD = 1;
            hevc.enableFillerDataInsertion = !recording && spec.constant_bitrate != 0;
            hevc.chromaFormatIDC = 1;
            hevc.inputBitDepth = spec.input_depth == 10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
            hevc.outputBitDepth = spec.output_depth == 10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
            colour = &hevc.hevcVUIParameters;
        }
        auto &vui = *colour;
        vui.videoSignalTypePresentFlag = vui.colourDescriptionPresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec;
        init.presetGUID = preset_guid;
        init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
        init.encodeWidth = init.darWidth = spec.width;
        init.encodeHeight = init.darHeight = spec.height;
        init.frameRateNum = spec.fps;
        init.frameRateDen = 1;
        init.enableEncodeAsync = init.enablePTD = 1;
        init.encodeConfig = &config;
        status = api.nvEncInitializeEncoder(session, &init);
        if (status != NV_ENC_SUCCESS) return fail(recording
            ? "initialize constant-QP video encoder with spatial AQ8" : "initialize video encoder", status);
        auto graphics = static_cast<ID3D11Device *>(device);
        graphics->GetImmediateContext(&context);
        int result = event(&eos);
        if (result) return result;
        eos_registered = true;
        for (auto &slot : slots) {
            result = resource(graphics, slot.input);
            if (result) return result;
            D3D11_QUERY_DESC query = { D3D11_QUERY_EVENT, 0 };
            HRESULT created = graphics->CreateQuery(&query, &slot.copy);
            if (FAILED(created)) {
                std::snprintf(error, sizeof(error), "create NVENC GPU copy query: HRESULT 0x%08lx", static_cast<unsigned long>(created));
                return internal_error;
            }
            NV_ENC_CREATE_BITSTREAM_BUFFER output = {};
            output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            status = api.nvEncCreateBitstreamBuffer(session, &output);
            if (status != NV_ENC_SUCCESS) return fail("allocate encoded output", status);
            slot.buffer = output.bitstreamBuffer;
            result = event(&slot.event);
            if (result) return result;
            slot.registered = true;
        }
        NV_ENC_SEQUENCE_PARAM_PAYLOAD sequence = {};
        sequence.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        sequence.spsppsBuffer = headers;
        sequence.inBufferSize = sizeof(headers);
        sequence.outSPSPPSPayloadSize = &headers_size;
        status = api.nvEncGetSequenceParams(session, &sequence);
        return status == NV_ENC_SUCCESS ? 0 : fail("read decoder configuration", status);
    }
};
} // namespace

extern "C" {
void *isocast_nvenc_open(void *device, const Config *config, char *error, size_t size) {
    auto encoder = new (std::nothrow) Encoder();
    if (!encoder) { std::snprintf(error, size, "allocate NVIDIA encoder state: out of memory"); return nullptr; }
    if (encoder->open(device, *config)) {
        std::snprintf(error, size, "%s", encoder->error);
        if (encoder->release() == 0) delete encoder;
        return nullptr;
    }
    return encoder;
}

const char *isocast_nvenc_error(Encoder *encoder) { return encoder->error; }
uint32_t isocast_nvenc_sessions() { return sessions.load(); }
const uint8_t *isocast_nvenc_headers(Encoder *encoder, uint32_t *size) {
    *size = encoder->headers_size;
    return encoder->headers;
}

// NVENC forbids client access while a resource is mapped. Capture/program
// frames also feed compositors, monitors and other encoders, so copy on GPU
// into this encoder's private slot before mapping it. Never map a shared
// source frame in place. A failed MapInputResource need not synchronize the
// copy: the caller must retain its source lease until wait_copy succeeds.
int isocast_nvenc_submit(Encoder *encoder, ID3D11Texture2D *texture, uint64_t pts, uint32_t keyframe) {
    encoder->rejected = false;
    if (encoder->count == slots_count) {
        encoder->rejected = true;
        return again;
    }
    auto &slot = encoder->slots[(encoder->head + encoder->count) % slots_count];
    auto &input = slot.input;
    if (input.mapped || slot.copying) {
        std::snprintf(encoder->error, sizeof(encoder->error), "NVENC input slot is still mapped or copying");
        return internal_error;
    }
    encoder->rejected = true;
    D3D11_TEXTURE2D_DESC desc = {};
    texture->GetDesc(&desc);
    if (desc.Width != encoder->init.encodeWidth || desc.Height != encoder->init.encodeHeight
        || desc.Format != encoder->texture_format || desc.ArraySize != 1
        || desc.MipLevels != 1 || desc.SampleDesc.Count != 1) {
        std::snprintf(encoder->error, sizeof(encoder->error), "input texture shape/format changed");
        return internal_error;
    }
    encoder->context->CopyResource(input.texture, texture);
    encoder->context->End(slot.copy);
    slot.copying = true;
    encoder->context->Flush();
    NV_ENC_MAP_INPUT_RESOURCE map = {};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = input.registered;
    auto status = encoder->api.nvEncMapInputResource(encoder->session, &map);
    if (status != NV_ENC_SUCCESS) return encoder->fail("map D3D11 input texture", status);
    input.mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS picture = {};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = encoder->init.encodeWidth;
    picture.inputHeight = encoder->init.encodeHeight;
    picture.inputTimeStamp = pts;
    picture.inputDuration = 1000000000ULL / encoder->init.frameRateNum;
    picture.inputBuffer = input.mapped;
    picture.bufferFmt = encoder->input_format;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.outputBitstream = slot.buffer;
    picture.completionEvent = slot.event;
    if (keyframe || !encoder->submitted)
        picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    encoder->rejected = false;
    status = encoder->api.nvEncEncodePicture(encoder->session, &picture);
    if (status != NV_ENC_SUCCESS && status != NV_ENC_ERR_NEED_MORE_INPUT) {
        auto unmapped = encoder->api.nvEncUnmapInputResource(encoder->session, input.mapped);
        if (unmapped != NV_ENC_SUCCESS) return encoder->fail("unmap refused input", unmapped);
        input.mapped = nullptr;
        encoder->rejected = status == NV_ENC_ERR_ENCODER_BUSY;
        return status == NV_ENC_ERR_ENCODER_BUSY ? again : encoder->fail("submit video frame", status);
    }
    slot.completed = false;
    ++encoder->count;
    encoder->submitted = true;
    return 0;
}

int isocast_nvenc_wait_copy(Encoder *encoder, uint32_t timeout_ms) {
    return encoder->wait_copy(timeout_ms);
}

uint32_t isocast_nvenc_rejected(Encoder *encoder) {
    return encoder->rejected ? 1 : 0;
}

// Borrowed handles. Only the completion worker consumes these auto-reset
// events, in submission order. The owner must detach from its wait set before
// closing the encoder. EOS replaces the picture event after the last picture.
void *isocast_nvenc_wait_event(Encoder *encoder, uint32_t *kind) {
    *kind = 0;
    if (encoder->count) {
        auto &slot = encoder->slots[encoder->head];
        if (slot.completed) return nullptr;
        *kind = 1;
        return slot.event;
    }
    if (encoder->flushing && !encoder->flushed) {
        *kind = 2;
        return encoder->eos;
    }
    return nullptr;
}

int isocast_nvenc_event_ready(Encoder *encoder, uint32_t kind) {
    if (kind == 1 && encoder->count) {
        encoder->slots[encoder->head].completed = true;
        return 0;
    }
    if (kind == 2 && !encoder->count && encoder->flushing) {
        encoder->flushed = true;
        return 0;
    }
    std::snprintf(encoder->error, sizeof(encoder->error), "unexpected NVENC completion event");
    return internal_error;
}

int isocast_nvenc_lock(Encoder *encoder, Packet *packet) {
    if (!encoder->count) return again;
    auto &slot = encoder->slots[encoder->head];
    if (!slot.completed) return again;
    // A successful submit keeps its source in Rust's pending FIFO. Prove the
    // copy here before that lease can retire, without stalling admission.
    int copied = encoder->copy_ready(slot);
    if (copied) return copied;
    NV_ENC_LOCK_BITSTREAM output = {};
    output.version = NV_ENC_LOCK_BITSTREAM_VER;
    output.outputBitstream = slot.buffer;
    output.doNotWait = 1;
    auto status = encoder->api.nvEncLockBitstream(encoder->session, &output);
    if (status == NV_ENC_ERR_LOCK_BUSY) return again;
    if (status != NV_ENC_SUCCESS) return encoder->fail("lock encoded output", status);
    slot.locked = true;
    packet->data = static_cast<const uint8_t *>(output.bitstreamBufferPtr);
    packet->size = output.bitstreamSizeInBytes;
    packet->pts = output.outputTimeStamp;
    packet->keyframe = output.pictureType == NV_ENC_PIC_TYPE_IDR;
    // hwEncodeStatus is a sub-frame progress value (2 means complete),
    // NOT NVENCSTATUS. Full-picture readiness is the completion event plus
    // the successful nvEncLockBitstream return checked above.
    return 0;
}

int isocast_nvenc_unlock(Encoder *encoder) {
    auto &slot = encoder->slots[encoder->head];
    auto status = encoder->api.nvEncUnlockBitstream(encoder->session, slot.buffer);
    if (status != NV_ENC_SUCCESS) return encoder->fail("unlock encoded output", status);
    slot.locked = false;
    auto &input = slot.input;
    status = encoder->api.nvEncUnmapInputResource(encoder->session, input.mapped);
    if (status != NV_ENC_SUCCESS) return encoder->fail("unmap completed input", status);
    input.mapped = nullptr;
    encoder->head = (encoder->head + 1) % slots_count;
    --encoder->count;
    return 0;
}

int isocast_nvenc_flush(Encoder *encoder) {
    if (encoder->flushing) return 0;
    NV_ENC_PIC_PARAMS picture = {};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
    picture.completionEvent = encoder->eos;
    auto status = encoder->api.nvEncEncodePicture(encoder->session, &picture);
    if (status == NV_ENC_ERR_ENCODER_BUSY) return again;
    if (status != NV_ENC_SUCCESS) return encoder->fail("flush video encoder", status);
    encoder->flushing = true;
    return 0;
}

int isocast_nvenc_set_rate(Encoder *encoder, uint32_t fps, uint32_t bitrate) {
    if (encoder->recording) {
        std::snprintf(encoder->error, sizeof(encoder->error), "recording settings cannot change; open a new encoder");
        return internal_error;
    }
    auto config = encoder->config;
    config.rcParams.averageBitRate = config.rcParams.maxBitRate = config.rcParams.vbvBufferSize = bitrate;
    NV_ENC_RECONFIGURE_PARAMS change = {};
    change.version = NV_ENC_RECONFIGURE_PARAMS_VER;
    change.reInitEncodeParams = encoder->init;
    change.reInitEncodeParams.frameRateNum = fps;
    change.reInitEncodeParams.encodeConfig = &config;
    auto status = encoder->api.nvEncReconfigureEncoder(encoder->session, &change);
    if (status != NV_ENC_SUCCESS) return encoder->fail("change encoder rate", status);
    encoder->config = config;
    encoder->init.frameRateNum = fps;
    return 0;
}

// Failure keeps the object/resources alive. Never release an input still
// being read by the GPU just to make teardown look successful.
int isocast_nvenc_close(Encoder *encoder) {
    if (encoder->count || (encoder->submitted && !encoder->flushed)) {
        std::snprintf(encoder->error, sizeof(encoder->error), "encoder still owns GPU inputs or has not completed its flush");
        return internal_error;
    }
    int status = encoder->release();
    if (status) return status;
    delete encoder;
    return 0;
}
}
