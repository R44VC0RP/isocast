#include "camera.h"
#include <algorithm>
#include <mutex>
#include <strsafe.h>

namespace {
constexpr REFERENCE_TIME second = 10000000;
struct Type {
    Format mode;
    bool yuy2;
    GUID subtype() const { return yuy2 ? MEDIASUBTYPE_YUY2 : MEDIASUBTYPE_NV12; }
    LONG length() const { return LONG(mode.width * mode.height * (yuy2 ? 4 : 3) / 2); }
    REFERENCE_TIME interval() const { return (second + mode.fps / 2) / mode.fps; }
    DWORD bitrate() const { return DWORD(std::min<uint64_t>(UINT32_MAX, uint64_t(length()) * 8 * mode.fps)); }
    HRESULT write(AM_MEDIA_TYPE *m) const {
        if (!m)
            return E_POINTER;
        *m = {};
        auto v = static_cast<VIDEOINFOHEADER *>(CoTaskMemAlloc(sizeof(VIDEOINFOHEADER)));
        if (!v)
            return E_OUTOFMEMORY;
        *v = {};
        v->AvgTimePerFrame = interval();
        v->dwBitRate = bitrate();
        auto &b = v->bmiHeader;
        b.biSize = sizeof(b);
        b.biWidth = mode.width;
        b.biHeight = mode.height;
        b.biPlanes = 1;
        b.biBitCount = yuy2 ? 16 : 12;
        b.biCompression = subtype().Data1;
        b.biSizeImage = length();
        m->majortype = MEDIATYPE_Video;
        m->subtype = subtype();
        m->bFixedSizeSamples = TRUE;
        m->lSampleSize = length();
        m->formattype = FORMAT_VideoInfo;
        m->cbFormat = sizeof(*v);
        m->pbFormat = reinterpret_cast<BYTE *>(v);
        return S_OK;
    }
    HRESULT allocate(AM_MEDIA_TYPE **out) const {
        if (!out)
            return E_POINTER;
        auto m = static_cast<AM_MEDIA_TYPE *>(CoTaskMemAlloc(sizeof(AM_MEDIA_TYPE)));
        *out = nullptr;
        if (!m)
            return E_OUTOFMEMORY;
        HRESULT hr = write(m);
        if (FAILED(hr))
            CoTaskMemFree(m);
        else
            *out = m;
        return hr;
    }
    bool matches(const AM_MEDIA_TYPE *m, bool partial = false) const {
        if (!m)
            return partial;
        auto match = [partial](REFGUID a, REFGUID b) { return a == b || (partial && a == GUID_NULL); };
        if (!match(m->majortype, MEDIATYPE_Video) || !match(m->subtype, subtype()) ||
            !match(m->formattype, FORMAT_VideoInfo))
            return false;
        if (!m->cbFormat)
            return partial;
        if (!m->pbFormat || m->cbFormat < sizeof(VIDEOINFOHEADER))
            return false;
        const auto &v = *reinterpret_cast<const VIDEOINFOHEADER *>(m->pbFormat);
        const auto &b = v.bmiHeader;
        return b.biWidth == LONG(mode.width) && b.biHeight == LONG(mode.height) &&
               b.biCompression == subtype().Data1 && b.biPlanes == 1 && b.biBitCount == (yuy2 ? 16 : 12) &&
               (!v.AvgTimePerFrame ||
                (v.AvgTimePerFrame >= interval() - 1 && v.AvgTimePerFrame <= interval() + 1));
    }
};

// Immutable snapshot, including the current first-choice media type. An
// enumerator retains its pin (and therefore the filter) through Reset/Clone.
template <class Interface, class Item> class Enum final : public Interface, public Counted {
    ComPtr<IPin> pin;
    Type type;
    ULONG pos, count;
    HRESULT item(ULONG, IPin **out) { return pin.CopyTo(out); }
    HRESULT item(ULONG i, AM_MEDIA_TYPE **out) {
        Type t = type;
        if (i)
            t.yuy2 = !t.yuy2;
        return t.allocate(out);
    }

  public:
    Enum(IPin *p, Type t, ULONG n, ULONG i = 0) : pin(p), type(t), pos(i), count(n) {}
    REFCOUNT()
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id, void **out) override {
        if (!out)
            return E_POINTER;
        *out = nullptr;
        if (id != IID_IUnknown && id != __uuidof(Interface))
            return E_NOINTERFACE;
        *out = static_cast<Interface *>(this);
        AddRef();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Next(ULONG n, Item **out, ULONG *got) override {
        if (!out || (!got && n != 1))
            return E_POINTER;
        ULONG i = 0;
        if (got)
            *got = 0;
        for (; i < n && pos < count; ++i, ++pos) {
            HRESULT hr = item(pos, out + i);
            if (FAILED(hr))
                return hr;
            if (got)
                ++*got;
        }
        return i == n ? S_OK : S_FALSE;
    }
    HRESULT STDMETHODCALLTYPE Skip(ULONG n) override {
        auto k = std::min(n, count - pos);
        pos += k;
        return n == k ? S_OK : S_FALSE;
    }
    HRESULT STDMETHODCALLTYPE Reset() override {
        pos = 0;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Clone(Interface **out) override {
        if (!out)
            return E_POINTER;
        *out = new (std::nothrow) Enum(pin.Get(), type, count, pos);
        return *out ? S_OK : E_OUTOFMEMORY;
    }
};

class Filter;
class Pin final : public IPin, public IAMStreamConfig, public IKsPropertySet {
    Filter &owner;

  public:
    explicit Pin(Filter &f) : owner(f) {}
    ULONG STDMETHODCALLTYPE AddRef() override;
    ULONG STDMETHODCALLTYPE Release() override;
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID, void **) override;
    HRESULT STDMETHODCALLTYPE Connect(IPin *, const AM_MEDIA_TYPE *) override;
    HRESULT STDMETHODCALLTYPE ReceiveConnection(IPin *, const AM_MEDIA_TYPE *) override {
        return E_UNEXPECTED;
    }
    HRESULT STDMETHODCALLTYPE Disconnect() override;
    HRESULT STDMETHODCALLTYPE ConnectedTo(IPin **) override;
    HRESULT STDMETHODCALLTYPE ConnectionMediaType(AM_MEDIA_TYPE *) override;
    HRESULT STDMETHODCALLTYPE QueryPinInfo(PIN_INFO *) override;
    HRESULT STDMETHODCALLTYPE QueryDirection(PIN_DIRECTION *d) override {
        if (!d)
            return E_POINTER;
        *d = PINDIR_OUTPUT;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryId(LPWSTR *s) override {
        if (!s)
            return E_POINTER;
        constexpr wchar_t id[] = L"Capture";
        *s = static_cast<LPWSTR>(CoTaskMemAlloc(sizeof(id)));
        if (!*s)
            return E_OUTOFMEMORY;
        memcpy(*s, id, sizeof(id));
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryAccept(const AM_MEDIA_TYPE *) override;
    HRESULT STDMETHODCALLTYPE EnumMediaTypes(IEnumMediaTypes **) override;
    HRESULT STDMETHODCALLTYPE QueryInternalConnections(IPin **, ULONG *) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE EndOfStream() override { return E_UNEXPECTED; }
    HRESULT STDMETHODCALLTYPE BeginFlush() override { return E_UNEXPECTED; }
    HRESULT STDMETHODCALLTYPE EndFlush() override { return E_UNEXPECTED; }
    HRESULT STDMETHODCALLTYPE NewSegment(REFERENCE_TIME, REFERENCE_TIME, double) override {
        return E_UNEXPECTED;
    }
    HRESULT STDMETHODCALLTYPE SetFormat(AM_MEDIA_TYPE *) override;
    HRESULT STDMETHODCALLTYPE GetFormat(AM_MEDIA_TYPE **) override;
    HRESULT STDMETHODCALLTYPE GetNumberOfCapabilities(int *n, int *size) override {
        if (!n || !size)
            return E_POINTER;
        *n = 2;
        *size = sizeof(VIDEO_STREAM_CONFIG_CAPS);
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetStreamCaps(int, AM_MEDIA_TYPE **, BYTE *) override;
    HRESULT STDMETHODCALLTYPE Set(REFGUID, DWORD, LPVOID, DWORD, LPVOID, DWORD) override { return E_NOTIMPL; }
    HRESULT STDMETHODCALLTYPE QuerySupported(REFGUID set, DWORD id, DWORD *support) override {
        if (!support)
            return E_POINTER;
        *support = 0;
        if (set != AMPROPSETID_Pin)
            return E_PROP_SET_UNSUPPORTED;
        if (id != AMPROPERTY_PIN_CATEGORY)
            return E_PROP_ID_UNSUPPORTED;
        *support = KSPROPERTY_SUPPORT_GET;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Get(REFGUID set, DWORD id, LPVOID, DWORD, LPVOID data, DWORD len,
                                  DWORD *used) override {
        DWORD flags{};
        HRESULT hr = QuerySupported(set, id, &flags);
        if (FAILED(hr))
            return hr;
        if (used)
            *used = sizeof(GUID);
        if (!data)
            return used ? S_OK : E_POINTER;
        if (len < sizeof(GUID))
            return HRESULT_FROM_WIN32(ERROR_MORE_DATA);
        memcpy(data, &PIN_CATEGORY_CAPTURE, sizeof(GUID));
        return S_OK;
    }
};

class Filter final : public IBaseFilter, public IAMFilterMiscFlags, public Counted {
    friend class Pin;
    Pin pin{*this};
    std::recursive_mutex lock; // pin negotiation may synchronously query this pin
    std::mutex transition;
    FILTER_STATE state = State_Stopped;
    Type type{camera_probe(), false};
    bool fixed = false;
    wchar_t name[MAX_FILTER_NAME] = L"Isocast Camera";
    IFilterGraph *graph = nullptr; // graph owns the filter; no reference cycle
    ComPtr<IPin> peer;
    ComPtr<IMemInputPin> input;
    ComPtr<IMemAllocator> allocator;
    ComPtr<IReferenceClock> clock, runClock;
    HANDLE stopEvent{}, thread{};
    Receiver *receiver{};
    REFERENCE_TIME origin{};

    // While the worker exists its pin, allocator, clock and type are immutable.
    // No filter mutex is held during GetBuffer, Receive, flush, or join.
    void deliver() {
        const auto wallStart = GetTickCount64();
        uint64_t index = 0;
        while (WaitForSingleObject(stopEvent, 0) == WAIT_TIMEOUT) {
            REFERENCE_TIME now{};
            if (runClock) {
                if (FAILED(runClock->GetTime(&now)))
                    break;
                now -= origin;
            } else
                now = REFERENCE_TIME(GetTickCount64() - wallStart) * 10000;
            const auto fps = type.mode.fps;
            auto at = [fps](uint64_t i) {
                return REFERENCE_TIME(i / fps) * second + REFERENCE_TIME(i % fps) * second / fps;
            };
            auto start = at(index);
            if (start > now) {
                auto ms = std::clamp<REFERENCE_TIME>((start - now + 9999) / 10000, 1, 1000);
                WaitForSingleObject(stopEvent, DWORD(ms));
                continue;
            }
            bool discontinuity = index == 0;
            if (now - start >= type.interval()) {
                index = uint64_t(now / second) * fps + uint64_t(now % second) * fps / second;
                start = at(index);
                discontinuity = true;
            }
            auto end = at(index + 1);
            ComPtr<IMediaSample> sample;
            if (FAILED(allocator->GetBuffer(&sample, nullptr, nullptr, 0)))
                break;
            BYTE *data{};
            if (FAILED(sample->GetPointer(&data)) || sample->GetSize() < type.length() ||
                !camera_fill(receiver, data, type.length()))
                break;
            sample->SetActualDataLength(type.length());
            sample->SetTime(&start, &end);
            sample->SetSyncPoint(TRUE);
            sample->SetPreroll(FALSE);
            sample->SetDiscontinuity(discontinuity);
            if (input->Receive(sample.Get()) != S_OK)
                break;
            ++index;
        }
    }
    static DWORD WINAPI entry(LPVOID data) {
        HRESULT init = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        static_cast<Filter *>(data)->deliver();
        if (SUCCEEDED(init))
            CoUninitialize();
        return 0;
    }
    void halt(FILTER_STATE next) {
        {
            std::lock_guard guard(lock);
            if (!thread) {
                state = next;
                return;
            }
        }
        SetEvent(stopEvent);
        peer->BeginFlush();
        allocator->Decommit();
        WaitForSingleObject(thread, INFINITE);
        peer->EndFlush();
        CloseHandle(thread);
        CloseHandle(stopEvent);
        camera_close(receiver);
        std::lock_guard guard(lock);
        receiver = nullptr;
        thread = nullptr;
        stopEvent = nullptr;
        runClock.Reset();
        state = next;
    }

  public:
    REFCOUNT()
    ~Filter() override { halt(State_Stopped); }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id, void **out) override {
        if (!out)
            return E_POINTER;
        *out = nullptr;
        if (id == IID_IUnknown || id == IID_IPersist || id == IID_IMediaFilter || id == IID_IBaseFilter)
            *out = static_cast<IBaseFilter *>(this);
        else if (id == IID_IAMFilterMiscFlags)
            *out = static_cast<IAMFilterMiscFlags *>(this);
        else
            return E_NOINTERFACE;
        AddRef();
        return S_OK;
    }
    ULONG STDMETHODCALLTYPE GetMiscFlags() override { return AM_FILTER_MISC_FLAGS_IS_SOURCE; }
    HRESULT STDMETHODCALLTYPE GetClassID(CLSID *id) override {
        if (!id)
            return E_POINTER;
        *id = camera_id;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Stop() override {
        std::lock_guard guard(transition);
        halt(State_Stopped);
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Pause() override {
        std::lock_guard guard(transition);
        halt(State_Paused);
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE Run(REFERENCE_TIME start) override {
        std::lock_guard control(transition);
        std::lock_guard guard(lock);
        if (state == State_Running)
            return S_OK;
        if (peer) {
            HRESULT hr = allocator->Commit();
            if (FAILED(hr))
                return hr;
            stopEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
            receiver = camera_open(type.mode, type.yuy2);
            runClock = clock;
            origin = start;
            if (stopEvent && receiver)
                thread = CreateThread(nullptr, 0, entry, this, 0, nullptr);
            if (!thread) {
                camera_close(receiver);
                receiver = nullptr;
                if (stopEvent)
                    CloseHandle(stopEvent);
                stopEvent = nullptr;
                runClock.Reset();
                allocator->Decommit();
                return E_OUTOFMEMORY;
            }
        }
        state = State_Running;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetState(DWORD, FILTER_STATE *s) override {
        if (!s)
            return E_POINTER;
        std::lock_guard guard(lock);
        *s = state;
        return state == State_Paused ? VFW_S_CANT_CUE : S_OK;
    }
    HRESULT STDMETHODCALLTYPE SetSyncSource(IReferenceClock *c) override {
        std::lock_guard guard(lock);
        clock = c;
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE GetSyncSource(IReferenceClock **c) override {
        if (!c)
            return E_POINTER;
        std::lock_guard guard(lock);
        return clock.CopyTo(c);
    }
    HRESULT STDMETHODCALLTYPE EnumPins(IEnumPins **out) override {
        if (!out)
            return E_POINTER;
        std::lock_guard guard(lock);
        *out = new (std::nothrow) Enum<IEnumPins, IPin>(&pin, type, 1);
        return *out ? S_OK : E_OUTOFMEMORY;
    }
    HRESULT STDMETHODCALLTYPE FindPin(LPCWSTR id, IPin **p) override {
        if (!id || !p)
            return E_POINTER;
        *p = nullptr;
        if (wcscmp(id, L"Capture"))
            return VFW_E_NOT_FOUND;
        *p = &pin;
        AddRef();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryFilterInfo(FILTER_INFO *info) override {
        if (!info)
            return E_POINTER;
        std::lock_guard guard(lock);
        StringCchCopyW(info->achName, MAX_FILTER_NAME, name);
        info->pGraph = graph;
        if (graph)
            graph->AddRef();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE JoinFilterGraph(IFilterGraph *g, LPCWSTR n) override {
        std::lock_guard guard(lock);
        graph = g;
        StringCchCopyNW(name, MAX_FILTER_NAME, n ? n : camera_name, MAX_FILTER_NAME - 1);
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE QueryVendorInfo(LPWSTR *v) override {
        if (v)
            *v = nullptr;
        return E_NOTIMPL;
    }
};

ULONG Pin::AddRef() { return owner.AddRef(); }
ULONG Pin::Release() { return owner.Release(); }
HRESULT Pin::QueryInterface(REFIID id, void **out) {
    if (!out)
        return E_POINTER;
    *out = nullptr;
    if (id == IID_IUnknown || id == IID_IPin)
        *out = static_cast<IPin *>(this);
    else if (id == IID_IAMStreamConfig)
        *out = static_cast<IAMStreamConfig *>(this);
    else if (id == IID_IKsPropertySet)
        *out = static_cast<IKsPropertySet *>(this);
    else
        return E_NOINTERFACE;
    AddRef();
    return S_OK;
}
HRESULT Pin::QueryPinInfo(PIN_INFO *info) {
    if (!info)
        return E_POINTER;
    info->pFilter = &owner;
    AddRef();
    info->dir = PINDIR_OUTPUT;
    StringCchCopyW(info->achName, ARRAYSIZE(info->achName), L"Capture");
    return S_OK;
}
HRESULT Pin::Connect(IPin *peer, const AM_MEDIA_TYPE *proposed) {
    if (!peer)
        return E_POINTER;
    std::lock_guard guard(owner.lock);
    if (owner.state != State_Stopped)
        return VFW_E_NOT_STOPPED;
    if (owner.peer)
        return VFW_E_ALREADY_CONNECTED;
    ComPtr<IMemInputPin> input;
    HRESULT hr = peer->QueryInterface(IID_PPV_ARGS(&input));
    if (FAILED(hr))
        return hr;
    Type t = owner.type;
    for (int i = 0; i < (owner.fixed ? 1 : 2); ++i, t.yuy2 = !t.yuy2) {
        if (!t.matches(proposed, true))
            continue;
        AM_MEDIA_TYPE m{};
        if (FAILED(hr = t.write(&m)))
            return hr;
        hr = peer->ReceiveConnection(this, &m);
        CoTaskMemFree(m.pbFormat);
        if (FAILED(hr))
            continue;
        ComPtr<IMemAllocator> allocator;
        if (FAILED(input->GetAllocator(&allocator)))
            hr = CoCreateInstance(CLSID_MemoryAllocator, nullptr, CLSCTX_INPROC_SERVER,
                                  IID_PPV_ARGS(&allocator));
        if (SUCCEEDED(hr)) {
            ALLOCATOR_PROPERTIES req{}, actual{};
            if (FAILED(input->GetAllocatorRequirements(&req)))
                req = {};
            req.cbBuffer = std::max(req.cbBuffer, t.length());
            req.cBuffers = std::max(req.cBuffers, 2L);
            req.cbAlign = std::max(req.cbAlign, 1L);
            hr = allocator->SetProperties(&req, &actual);
            if (SUCCEEDED(hr) && actual.cbBuffer < t.length())
                hr = VFW_E_BUFFER_OVERFLOW;
            if (SUCCEEDED(hr))
                hr = input->NotifyAllocator(allocator.Get(), FALSE);
        }
        if (FAILED(hr)) {
            peer->Disconnect();
            return hr;
        }
        owner.type = t;
        owner.peer = peer;
        owner.input = input;
        owner.allocator = allocator;
        return S_OK;
    }
    return VFW_E_NO_ACCEPTABLE_TYPES;
}
HRESULT Pin::Disconnect() {
    std::lock_guard guard(owner.lock);
    if (owner.state != State_Stopped)
        return VFW_E_NOT_STOPPED;
    if (!owner.peer)
        return S_FALSE;
    owner.allocator.Reset();
    owner.input.Reset();
    owner.peer.Reset();
    return S_OK;
}
HRESULT Pin::ConnectedTo(IPin **p) {
    if (!p)
        return E_POINTER;
    std::lock_guard guard(owner.lock);
    *p = nullptr;
    return owner.peer ? owner.peer.CopyTo(p) : VFW_E_NOT_CONNECTED;
}
HRESULT Pin::ConnectionMediaType(AM_MEDIA_TYPE *m) {
    if (!m)
        return E_POINTER;
    std::lock_guard guard(owner.lock);
    return owner.peer ? owner.type.write(m) : VFW_E_NOT_CONNECTED;
}
HRESULT Pin::QueryAccept(const AM_MEDIA_TYPE *m) {
    std::lock_guard guard(owner.lock);
    auto t = owner.type;
    if (t.matches(m))
        return S_OK;
    t.yuy2 = !t.yuy2;
    return !owner.fixed && t.matches(m) ? S_OK : S_FALSE;
}
HRESULT Pin::EnumMediaTypes(IEnumMediaTypes **out) {
    if (!out)
        return E_POINTER;
    std::lock_guard guard(owner.lock);
    *out = new (std::nothrow) Enum<IEnumMediaTypes, AM_MEDIA_TYPE>(this, owner.type, owner.fixed ? 1 : 2);
    return *out ? S_OK : E_OUTOFMEMORY;
}
HRESULT Pin::SetFormat(AM_MEDIA_TYPE *m) {
    std::lock_guard guard(owner.lock);
    if (owner.state != State_Stopped)
        return VFW_E_NOT_STOPPED;
    if (owner.peer)
        return VFW_E_ALREADY_CONNECTED;
    if (!m) {
        owner.fixed = false;
        owner.type.yuy2 = false;
        return S_OK;
    }
    Type t = owner.type;
    t.yuy2 = (m->subtype == MEDIASUBTYPE_YUY2);
    if (!t.matches(m))
        return VFW_E_INVALIDMEDIATYPE;
    owner.type = t;
    owner.fixed = true;
    return S_OK;
}
HRESULT Pin::GetFormat(AM_MEDIA_TYPE **out) {
    std::lock_guard guard(owner.lock);
    return owner.type.allocate(out);
}
HRESULT Pin::GetStreamCaps(int index, AM_MEDIA_TYPE **out, BYTE *caps) {
    if (!out || !caps)
        return E_POINTER;
    *out = nullptr;
    if (index < 0 || index >= 2)
        return S_FALSE;
    std::lock_guard guard(owner.lock);
    Type t = owner.type;
    t.yuy2 = (index == 1);
    VIDEO_STREAM_CONFIG_CAPS c{};
    c.guid = FORMAT_VideoInfo;
    c.InputSize = {LONG(t.mode.width), LONG(t.mode.height)};
    c.MinCroppingSize = c.MaxCroppingSize = c.MinOutputSize = c.MaxOutputSize = c.InputSize;
    c.MinFrameInterval = c.MaxFrameInterval = t.interval();
    c.MinBitsPerSecond = c.MaxBitsPerSecond = LONG(std::min<DWORD>(INT32_MAX, t.bitrate()));
    memcpy(caps, &c, sizeof(c));
    return t.allocate(out);
}
} // namespace

HRESULT create_camera(REFIID id, void **out) {
    auto f = new (std::nothrow) Filter;
    if (!f)
        return E_OUTOFMEMORY;
    HRESULT hr = f->QueryInterface(id, out);
    f->Release();
    return hr;
}
