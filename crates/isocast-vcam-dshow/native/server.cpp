#include "camera.h"
#include <strsafe.h>

class Factory final : public IClassFactory, public Counted {
  public:
    REFCOUNT()
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id, void **out) override {
        if (!out)
            return E_POINTER;
        *out = nullptr;
        if (id != IID_IUnknown && id != IID_IClassFactory)
            return E_NOINTERFACE;
        *out = static_cast<IClassFactory *>(this);
        AddRef();
        return S_OK;
    }
    HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown *outer, REFIID id, void **out) override {
        if (!out)
            return E_POINTER;
        *out = nullptr;
        return outer ? CLASS_E_NOAGGREGATION : create_camera(id, out);
    }
    HRESULT STDMETHODCALLTYPE LockServer(BOOL lock) override {
        if (lock)
            ++camera_objects;
        else
            --camera_objects;
        return S_OK;
    }
};
STDAPI DllGetClassObject(REFCLSID cls, REFIID id, void **out) {
    if (!out)
        return E_POINTER;
    *out = nullptr;
    if (cls != camera_id)
        return CLASS_E_CLASSNOTAVAILABLE;
    auto f = new (std::nothrow) Factory;
    if (!f)
        return E_OUTOFMEMORY;
    HRESULT hr = f->QueryInterface(id, out);
    f->Release();
    return hr;
}
STDAPI DllCanUnloadNow() { return camera_objects == 0 ? S_OK : S_FALSE; }

// Explicit HKCU rather than ambiguous writes to HKCR. Use the registrar's
// native view; the x86 and x64 DLLs never replace one another. Do not remove
// the camera category, its Instance key, or another install's class path.
struct Key {
    HKEY k{};
    ~Key() {
        if (k)
            RegCloseKey(k);
    }
    HRESULT create(const wchar_t *path) {
        return HRESULT_FROM_WIN32(
            RegCreateKeyExW(HKEY_CURRENT_USER, path, 0, nullptr, 0, KEY_WRITE, nullptr, &k, nullptr));
    }
    HRESULT text(const wchar_t *key, const wchar_t *value) {
        return HRESULT_FROM_WIN32(RegSetValueExW(k, key, 0, REG_SZ, reinterpret_cast<const BYTE *>(value),
                                                 DWORD((wcslen(value) + 1) * sizeof(wchar_t))));
    }
};
static HRESULT remove_key(const wchar_t *path) {
    auto err = RegDeleteTreeW(HKEY_CURRENT_USER, path);
    return err == ERROR_FILE_NOT_FOUND || err == ERROR_PATH_NOT_FOUND ? S_OK : HRESULT_FROM_WIN32(err);
}
static HRESULT camera_register(bool install) {
    HMODULE module{};
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                                GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            reinterpret_cast<LPCWSTR>(&camera_register), &module))
        return HRESULT_FROM_WIN32(GetLastError());
    wchar_t dll[32768];
    DWORD n = GetModuleFileNameW(module, dll, ARRAYSIZE(dll));
    if (!n || n >= ARRAYSIZE(dll))
        return HRESULT_FROM_WIN32(ERROR_BAD_PATHNAME);
    wchar_t id[40], category[40], cls[120], server[150], instance[170];
    StringFromGUID2(camera_id, id, ARRAYSIZE(id));
    StringFromGUID2(CLSID_VideoInputDeviceCategory, category, ARRAYSIZE(category));
    StringCchPrintfW(cls, ARRAYSIZE(cls), L"Software\\Classes\\CLSID\\%s", id);
    StringCchPrintfW(server, ARRAYSIZE(server), L"%s\\InprocServer32", cls);
    StringCchPrintfW(instance, ARRAYSIZE(instance), L"Software\\Classes\\CLSID\\%s\\Instance\\%s", category,
                     id);
    wchar_t registered[32768];
    DWORD size = sizeof(registered);
    auto err = RegGetValueW(HKEY_CURRENT_USER, server, nullptr, RRF_RT_REG_SZ, nullptr, registered, &size);
    if (err == ERROR_SUCCESS && _wcsicmp(dll, registered))
        return HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS);
    if (err != ERROR_SUCCESS && err != ERROR_FILE_NOT_FOUND && err != ERROR_PATH_NOT_FOUND)
        return HRESULT_FROM_WIN32(err);
    if (!install) {
        if (err != ERROR_SUCCESS)
            return S_OK;
        HRESULT hr = remove_key(instance);
        return FAILED(hr) ? hr : remove_key(cls);
    }
    // This is an explicitly picked capture source, not a candidate for
    // Intelligent Connect. Device enumeration needs its category moniker;
    // applications query the live types through the pin's IAMStreamConfig.
    HRESULT hr;
    {
        Key c, s, i;
        if (SUCCEEDED(hr = c.create(cls)) && SUCCEEDED(hr = c.text(nullptr, camera_name)) &&
            SUCCEEDED(hr = s.create(server)) && SUCCEEDED(hr = s.text(nullptr, dll)) &&
            SUCCEEDED(hr = s.text(L"ThreadingModel", L"Both")) && SUCCEEDED(hr = i.create(instance)) &&
            SUCCEEDED(hr = i.text(L"FriendlyName", camera_name)))
            hr = i.text(L"CLSID", id);
    }
    if (FAILED(hr) && err != ERROR_SUCCESS) {
        remove_key(instance);
        remove_key(cls);
    }
    return hr;
}
STDAPI DllRegisterServer() { return camera_register(true); }
STDAPI DllUnregisterServer() { return camera_register(false); }
