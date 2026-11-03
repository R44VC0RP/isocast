#pragma once
#include <windows.h>
#include <dshow.h>
#include <ks.h>
#include <ksmedia.h>
#include <wrl/client.h>
#include <atomic>
#include <cstdint>
#include <new>

using Microsoft::WRL::ComPtr;
inline constexpr GUID camera_id = {
    0x56e72df7, 0x95ef, 0x42fd, {0x95, 0xb6, 0x44, 0x91, 0xb6, 0x25, 0x82, 0x6b}};
inline constexpr wchar_t camera_name[] = L"Isocast Camera";
inline std::atomic<long> camera_objects{};

// Each independent object (including an enumerator or class factory) keeps
// the DLL loaded. The embedded output pin delegates its lifetime to the filter.
struct Counted {
    std::atomic<ULONG> refs{1};
    Counted() { ++camera_objects; }
    virtual ~Counted() { --camera_objects; }
};
#define REFCOUNT()                                                                                           \
    ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }                                             \
    ULONG STDMETHODCALLTYPE Release() override {                                                             \
        auto n = --refs;                                                                                     \
        if (!n)                                                                                              \
            delete this;                                                                                     \
        return n;                                                                                            \
    }

struct Format {
    uint32_t width, height, fps;
};
struct Receiver;
extern "C" Format camera_probe();
extern "C" Receiver *camera_open(Format, bool yuy2);
extern "C" bool camera_fill(Receiver *, BYTE *, size_t);
extern "C" void camera_close(Receiver *);
HRESULT create_camera(REFIID, void **);
