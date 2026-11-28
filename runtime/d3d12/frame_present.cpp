// Verify the visible window in screen coordinates, independently of GPU staging readbacks.
#include "frame_abi.h"
#include "frame_present.h"
#include <stdexcept>
#include <cstdio>
#include <future>

namespace tuxbox_test {
namespace {
void pump() {
    MSG message;
    while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE)) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

// Sample points in percent of the client area: four corners, then the centre.
constexpr std::array<POINT, 5> samples{{{5, 5}, {95, 5}, {5, 95}, {95, 95}, {50, 50}}};
// build.py writes the package splash beside the splash-mode guest: a 64x36
// image in this colour.
constexpr COLORREF splash_image = RGB(0x30, 0xc0, 0x90);
constexpr LONG splash_width = 64, splash_height = 36;

// GetPixel on the screen reads physical pixels, but a guest that is not DPI aware,
// like a title, places its windows in logical pixels. They differ once Windows
// scales the display, so move each sample point to the pixel it covers.
COLORREF screen_pixel(HDC screen, HWND window, POINT at) {
    using Convert = BOOL(WINAPI*)(HWND, POINT*);
    static const auto convert = reinterpret_cast<Convert>(
        GetProcAddress(GetModuleHandleW(L"user32.dll"), "LogicalToPhysicalPointForPerMonitorDPI"));
    if (convert) convert(window, &at);
    return GetPixel(screen, at.x, at.y);
}

std::string describe(const std::array<COLORREF, samples.size()>& pixels) {
    char text[80];
    std::snprintf(text, sizeof(text), " screen=%#lx/%#lx/%#lx/%#lx/%#lx", pixels[0], pixels[1], pixels[2],
                  pixels[3], pixels[4]);
    return text;
}
}

DisplayCheck::DisplayCheck(UINT width, UINT height, const wchar_t* window_mode)
    : check_size_(window_mode != nullptr), splash_(window_mode && !wcscmp(window_mode, L"splash")) {
    if (window_mode && !wcscmp(window_mode, L"owned")) return;
    if (splash_) {
        // Like ProjectScarlett: an ANSI class, and a window for the whole output.
        WNDCLASSEXA title{};
        title.cbSize = sizeof(title);
        title.lpfnWndProc = DefWindowProcA;
        title.hInstance = GetModuleHandleW(nullptr);
        title.lpszClassName = "TuxBoxSplashTitle";
        if (!RegisterClassExA(&title)) throw std::runtime_error("could not register the title window class");
        RECT frame{0, 0, static_cast<LONG>(width), static_cast<LONG>(height)};
        AdjustWindowRect(&frame, WS_OVERLAPPEDWINDOW, FALSE);
        window_ = CreateWindowExA(0, title.lpszClassName, "Project TuxBox splash check", WS_OVERLAPPEDWINDOW,
                                  CW_USEDEFAULT, CW_USEDEFAULT, frame.right - frame.left, frame.bottom - frame.top,
                                  nullptr, nullptr, title.hInstance, nullptr);
        if (!window_) throw std::runtime_error("could not create the title window");
        owns_title_ = true;
        // The provider fits the window as it is created and paints the package
        // image into it until frames come steadily.
        RECT client{};
        POINT origin{};
        measure(client, origin);
        ShowWindow(window_, SW_SHOW);
        UpdateWindow(window_);
        if (!splash_shows(4000)) throw std::runtime_error("the package splash did not appear" + describe(sample()) + sizing_);
        if (FindWindowW(L"TuxBox.Splash", nullptr)) throw std::runtime_error("the package splash opened a window of its own");
        return;
    }
    UINT client_width = width * 6, client_height = height * 6;
    if (check_size_) {
        client_width = !wcscmp(window_mode, L"fit") ? 960 : width;
        client_height = !wcscmp(window_mode, L"fit") ? 540 : height;
    }
    // The popup's client area fills the window. Keep it above other windows while sampling.
    window_ = CreateWindowExW(WS_EX_TOPMOST, L"STATIC", L"Project TuxBox GPU check",
                              WS_POPUP, 24, 24, client_width, client_height, nullptr, nullptr,
                              GetModuleHandleW(nullptr), nullptr);
    if (!window_) throw std::runtime_error("could not create presentation window");
    owns_title_ = true;
    ShowWindow(window_, SW_SHOW);
    SetActiveWindow(window_);
    UpdateWindow(window_);
    pump();
}
DisplayCheck::~DisplayCheck() { if (owns_title_) DestroyWindow(window_); }

void DisplayCheck::measure(RECT& client, POINT& origin) {
    if (!GetClientRect(window_, &client)) throw std::runtime_error("could not get presentation client area");
    origin = {};
    POINT end{client.right, client.bottom};
    if (!ClientToScreen(window_, &origin) || !ClientToScreen(window_, &end))
        throw std::runtime_error("could not locate presentation window");
    if (check_size_) {
        MONITORINFO monitor{};
        monitor.cbSize = sizeof(monitor);
        if (!GetMonitorInfoW(MonitorFromWindow(window_, MONITOR_DEFAULTTONEAREST), &monitor))
            throw std::runtime_error("could not locate display work area");
        const auto& area = monitor.rcWork;
        sizing_ = " client=" + std::to_string(client.right) + "x" + std::to_string(client.bottom)
            + " origin=" + std::to_string(origin.x) + "," + std::to_string(origin.y)
            + " monitor=" + std::to_string(area.right - area.left) + "x" + std::to_string(area.bottom - area.top);
        if (origin.x < area.left || origin.y < area.top || end.x > area.right || end.y > area.bottom)
            throw std::runtime_error("off-screen presentation" + sizing_);
        if (std::abs(client.right * 9 - client.bottom * 16) > 16)
            throw std::runtime_error("distorted presentation" + sizing_);
    }
}

std::array<COLORREF, 5> DisplayCheck::sample() const {
    std::array<COLORREF, samples.size()> pixels{};
    RECT client{};
    POINT origin{};
    if (!GetClientRect(window_, &client) || !ClientToScreen(window_, &origin)) return pixels;
    HDC screen = GetDC(nullptr);
    if (!screen) throw std::runtime_error("could not acquire screen DC");
    for (size_t i = 0; i < pixels.size(); ++i)
        pixels[i] = screen_pixel(screen, window_, {origin.x + client.right * samples[i].x / 100,
                                                   origin.y + client.bottom * samples[i].y / 100});
    ReleaseDC(nullptr, screen);
    return pixels;
}

bool DisplayCheck::shows(COLORREF corner, COLORREF center, ULONGLONG milliseconds) const {
    const auto deadline = GetTickCount64() + milliseconds;
    do {
        pump();
        const auto pixels = sample();
        bool matched = true;
        for (size_t i = 0; i < pixels.size(); ++i) matched &= pixels[i] == (i == 4 ? center : corner);
        if (matched) return true;
        Sleep(16);
    } while (GetTickCount64() < deadline);
    return false;
}

bool DisplayCheck::splash_shows(ULONGLONG milliseconds) const {
    const auto deadline = GetTickCount64() + milliseconds;
    do {
        pump();
        RECT client{};
        POINT origin{};
        if (!GetClientRect(window_, &client) || !ClientToScreen(window_, &origin)) return false;
        HDC screen = GetDC(nullptr);
        if (!screen) throw std::runtime_error("could not acquire screen DC");
        // The image fills the client area in its own proportions, centred.
        const bool tall = splash_width * client.bottom > splash_height * client.right;
        const LONG w = tall ? client.right : client.bottom * splash_width / splash_height;
        const LONG h = tall ? client.right * splash_height / splash_width : client.bottom;
        const POINT corner{origin.x + (client.right - w) / 2, origin.y + (client.bottom - h) / 2};
        bool matched = true;
        for (const auto& at : samples) {
            const POINT inside{corner.x + w * at.x / 100, corner.y + h * at.y / 100};
            matched &= screen_pixel(screen, window_, inside) == splash_image;
        }
        // Where the proportions differ, the bars beside or above it are black.
        if (client.right - w > 4 || client.bottom - h > 4)
            matched &= screen_pixel(screen, window_, {origin.x + 1, origin.y + 1}) == RGB(0, 0, 0);
        ReleaseDC(nullptr, screen);
        if (matched) return true;
        Sleep(16);
    } while (GetTickCount64() < deadline);
    return false;
}

void DisplayCheck::verify(ID3D12CommandQueue* queue, ID3D12Resource* source, UINT64 token,
                          COLORREF corner, COLORREF center, ID3D12Resource* overlay) {
    struct FullPlane {
        frame_abi::Plane prefix;
        UINT options[12];
    } planes[2]{{{token, 1, 0, &source}, {}}, {{token, 1, 0, &overlay}, {}}};
    auto& plane = planes[0];
    const UINT count = overlay ? 2 : 1;
    struct Parameters {
        float threshold;
        UINT options[7];
    } params{token == 1 ? 10.f : 0.f, {}};
    static_assert(sizeof(FullPlane) == 72 && sizeof(Parameters) == 32);
    const auto present = frame_abi::method<frame_abi::Present>(queue, 30);
    if (token == 1) {
        params.options[5] = 1;
        if (present(queue, 1, &plane.prefix, &params) != E_NOTIMPL)
            throw std::runtime_error("unsupported presentation flag was accepted");
        params.options[5] = 0;
        // Bytes 24 and 32 of the 72-byte retail plane are the source-rectangle and
        // destination pointers; the provider dereferences them. Probe the color
        // space at byte 40 instead, a plain value it must reject when nonzero.
        plane.options[4] = 1;
        if (present(queue, 1, &plane.prefix, &params) != E_NOTIMPL)
            throw std::runtime_error("unsupported plane option was accepted");
        plane.options[4] = 0;
    }
    // Frosty's main thread owns the game window while a render thread presents.
    const auto submit = [&] {
        auto render = std::async(std::launch::async, [&] { return present(queue, count, &plane.prefix, &params); });
        while (render.wait_for(std::chrono::milliseconds(1)) != std::future_status::ready) pump();
        // The present may resize the title window asynchronously just before it returns; apply
        // that move before the client area is measured.
        pump();
        const HRESULT hr = render.get();
        if (hr != S_OK) {
            char message[96];
            std::snprintf(message, sizeof(message), "queue slot 30 PresentX returned %#lx", static_cast<unsigned long>(hr));
            throw std::runtime_error(message);
        }
    };
    if (splash_ && token == 1) {
        // A lone first frame completes behind the splash, which stays on the screen
        // in the title's window.
        submit();
        const auto until = GetTickCount64() + 300;
        while (GetTickCount64() < until) {
            if (!splash_shows(0)) throw std::runtime_error("the first frame ended the package splash");
            Sleep(16);
        }
        return;
    }
    if (splash_) {
        // With the held first frame, three more within a second are steady and end the splash.
        for (int i = 0; i < 3; ++i) submit();
    }
    submit();
    if (!window_) {
        EnumWindows([](HWND w, LPARAM data) -> BOOL {
            DWORD pid{};
            GetWindowThreadProcessId(w, &pid);
            if (pid != GetCurrentProcessId() || !IsWindowVisible(w) || GetWindow(w, GW_OWNER)) return TRUE;
            *reinterpret_cast<HWND*>(data) = w;
            return FALSE;
        }, reinterpret_cast<LPARAM>(&window_));
    }
    RECT client{};
    POINT origin{};
    measure(client, origin);
    const auto deadline = GetTickCount64() + 4000;
    // GE-Proton11-14 can leave a presented image off the screen until a later
    // present, which a title supplies every frame. Repeat this one a few times.
    auto resubmit_at = GetTickCount64() + 250;
    int resubmits = 3;
    std::array<COLORREF, samples.size()> pixels{};
    do {
        pump();
        pixels = sample();
        bool matched = true;
        for (size_t i = 0; i < pixels.size(); ++i) matched &= pixels[i] == (i == 4 ? center : corner);
        if (matched) {
            if (splash_) {
                // Frames after that still reach the screen.
                for (int i = 0; i < 10; ++i) {
                    submit();
                    Sleep(16);
                }
                if (!shows(corner, center, 2000))
                    throw std::runtime_error("frames stopped reaching the screen after the splash" + describe(sample()));
                sizing_ += " splash=held,ended";
            }
            return;
        }
        if (resubmits && GetTickCount64() >= resubmit_at) {
            submit();
            --resubmits;
            resubmit_at += 250;
        }
        Sleep(16);
    } while (GetTickCount64() < deadline);
    char message[180];
    std::snprintf(message, sizeof(message), "screen colors on frame %llu: %#lx/%#lx/%#lx/%#lx/%#lx, expected %#lx border and %#lx center",
                  token, pixels[0], pixels[1], pixels[2], pixels[3], pixels[4], corner, center);
    throw std::runtime_error(message + sizing_);
}
}
