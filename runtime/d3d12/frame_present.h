#pragma once
#include <array>
#include <string>
#include <windows.h>
#include <d3d12.h>

namespace tuxbox_test {
class DisplayCheck {
public:
    DisplayCheck(UINT width, UINT height, const wchar_t* window_mode = nullptr);
    ~DisplayCheck();
    const std::string& sizing() const { return sizing_; }
    DisplayCheck(const DisplayCheck&) = delete;
    DisplayCheck& operator=(const DisplayCheck&) = delete;
    // With an overlay, each frame is presented as two planes: source, then the
    // overlay blended over it.
    void verify(ID3D12CommandQueue* queue, ID3D12Resource* source, UINT64 token,
                COLORREF corner, COLORREF center, ID3D12Resource* overlay = nullptr);
private:
    void measure(RECT& client, POINT& origin);
    std::array<COLORREF, 5> sample() const;
    bool shows(COLORREF corner, COLORREF center, ULONGLONG milliseconds) const;
    bool splash_shows(ULONGLONG milliseconds) const;
    HWND window_ = nullptr;
    bool owns_title_ = false;
    bool check_size_ = false;
    bool splash_ = false;
    // Whether a covered window was raised above the others (see raise_if_covered).
    mutable bool raised_ = false;
    std::string sizing_;
};
}
