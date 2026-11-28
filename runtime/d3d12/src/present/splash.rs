//! The package splash, shown until the title presents steadily.
//!
//! The console shows the `SplashScreenImage` from a title's `ShellVisuals`
//! from launch until the title presents. On the desktop that time showed the
//! title's own window, unpainted. ProjectScarlett presents a frame about two
//! seconds after creating it and another within the next 26 seconds, and
//! pumps no window messages in between.
//!
//! An in-context WinEvent hook runs on the thread that creates the title's
//! window, before the title shows it, and fits the window as the first present
//! would. A thread of TuxBox's own then paints the image into that window,
//! scaled to fill it in proportion, until the title presents steadily. Until then its frames
//! complete without reaching the screen, so no swap chain exists yet. A second
//! window would need the title's to be transparent behind it, which not every
//! desktop honours, and a tiling compositor places the two apart.

// Test builds have no attach initializer, so nothing starts the splash there.
#![cfg_attr(test, allow(dead_code))]
use super::window;
use std::collections::VecDeque;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW, GetModuleHandleW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result};

/// Series X|S titles present 16:9 frames. The first present fits the window
/// to its back buffer in the same proportion, so it leaves this size alone.
const CONSOLE_OUTPUT: (u32, u32) = (3840, 2160);
/// Frames count as steady once this many arrive within `STEADY_WINDOW`.
const STEADY_FRAMES: usize = 4;
const STEADY_WINDOW: Duration = Duration::from_secs(1);
/// A title that never presents steadily still reaches the screen.
const LONGEST: Duration = Duration::from_secs(60);
const LARGEST_IMAGE: u32 = 8192;
/// How often the image is painted again. The title's own thread does not
/// answer paint requests while the splash is up.
const REPAINT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Waiting for the title to create its window.
    Waiting,
    /// The hook is preparing a title window.
    Fitting,
    /// The title window is chosen and not yet painted.
    Claimed,
    /// The image is painted and the title's frames stay behind it.
    Shown,
    /// The title's frames go to the swap chain.
    Released,
}

struct State {
    stage: Stage,
    /// The title's window.
    title: Option<usize>,
}

struct Splash {
    state: Mutex<State>,
    changed: Condvar,
    /// Held while painting, so no paint is under way once a release has it.
    painting: Mutex<()>,
    pacing: Mutex<Pacing>,
}

struct Art {
    width: u32,
    height: u32,
    /// Top-down premultiplied BGRA rows.
    pixels: Vec<u8>,
}

static SPLASH: OnceLock<Splash> = OnceLock::new();

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The splash image a package declares, relative to the package.
pub(super) fn declaration(config: &str) -> Option<String> {
    let start = config.find("<ShellVisuals")? + "<ShellVisuals".len();
    let element = &config[start..];
    let element = &element[..element.find('>')?];
    attributes(element)
        .find(|&(name, _)| name == "SplashScreenImage")
        .map(|(_, image)| image.to_owned())
        .filter(|image| !image.is_empty())
}

/// The `name="value"` pairs of one XML start tag, without entity decoding.
fn attributes(mut rest: &str) -> impl Iterator<Item = (&str, &str)> {
    std::iter::from_fn(move || {
        let (name, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let quote = after.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let (value, tail) = after[1..].split_once(quote)?;
        rest = tail;
        Some((name.split_whitespace().last().unwrap_or(""), value))
    })
}

/// Where the image goes in an area: as large as fits in proportion, centred.
/// A window of the image's proportions shows no border, whatever its size.
pub(super) fn placement(area: (i32, i32), image: (u32, u32)) -> RECT {
    let (aw, ah) = (i64::from(area.0.max(0)), i64::from(area.1.max(0)));
    let (iw, ih) = (i64::from(image.0.max(1)), i64::from(image.1.max(1)));
    let (w, h) = if iw * ah > ih * aw {
        (aw, ih * aw / iw)
    } else {
        (iw * ah / ih, ah)
    };
    let (left, top) = ((aw - w) / 2, (ah - h) / 2);
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + w) as i32,
        bottom: (top + h) as i32,
    }
}

/// Each output index's source indices and weights, from `from` to `to`
/// samples: the area it covers when reducing, the two nearest samples when
/// enlarging. The weights of one output sum to 1.
fn taps(from: u32, to: u32) -> Vec<Vec<(usize, f32)>> {
    let scale = f64::from(from) / f64::from(to);
    (0..to)
        .map(|i| {
            if to > from {
                // The output sample's centre, in source sample centres.
                let at = ((f64::from(i) + 0.5) * scale - 0.5).clamp(0.0, f64::from(from - 1));
                let low = at.floor() as usize;
                let part = (at - low as f64) as f32;
                return if part > 0.0 {
                    vec![(low, 1.0 - part), (low + 1, part)]
                } else {
                    vec![(low, 1.0)]
                };
            }
            let (start, end) = (f64::from(i) * scale, f64::from(i + 1) * scale);
            (start.floor() as usize..(end.ceil() as usize).min(from as usize))
                .filter_map(|s| {
                    let cover = end.min(s as f64 + 1.0) - start.max(s as f64);
                    (cover > 0.0).then_some((s, (cover / scale) as f32))
                })
                .collect()
        })
        .collect()
}

/// Scale premultiplied BGRA pixels, averaging the area each output pixel
/// covers when reducing and interpolating when enlarging. Premultiplied
/// channels average correctly with their alpha.
pub(super) fn resize(pixels: &[u8], from: (u32, u32), to: (u32, u32)) -> Option<Vec<u8>> {
    let (fw, fh, tw, th) = (from.0 as usize, from.1 as usize, to.0 as usize, to.1 as usize);
    let average = |sources: &mut dyn Iterator<Item = ([u8; 4], f32)>| {
        let mut sum = [0f32; 4];
        for (pixel, weight) in sources {
            for (total, channel) in sum.iter_mut().zip(pixel) {
                *total += f32::from(channel) * weight;
            }
        }
        sum.map(|total| total.round().clamp(0.0, 255.0) as u8)
    };
    let pixel = |data: &[u8], index: usize| -> [u8; 4] { data[index * 4..][..4].try_into().unwrap() };
    let mut wide = Vec::new();
    wide.try_reserve_exact(fh * tw * 4).ok()?;
    let columns = taps(from.0, to.0);
    for y in 0..fh {
        for taps in &columns {
            wide.extend(average(&mut taps.iter().map(|&(x, w)| (pixel(pixels, y * fw + x), w))));
        }
    }
    let mut result = Vec::new();
    result.try_reserve_exact(tw * th * 4).ok()?;
    for taps in taps(from.1, to.1) {
        for x in 0..tw {
            result.extend(average(&mut taps.iter().map(|&(y, w)| (pixel(&wide, y * tw + x), w))));
        }
    }
    Some(result)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Hold,
    Steady,
    Timeout,
}

/// When the title's frames stop needing the splash.
#[derive(Default)]
pub(super) struct Pacing {
    first: Option<Instant>,
    recent: VecDeque<Instant>,
    frames: u64,
}

impl Pacing {
    pub(super) fn arrive(&mut self, now: Instant) -> Verdict {
        let first = *self.first.get_or_insert(now);
        self.frames += 1;
        self.recent.push_back(now);
        if self.recent.len() > STEADY_FRAMES {
            self.recent.pop_front();
        }
        if self.recent.len() == STEADY_FRAMES
            && self.recent.front().is_some_and(|&oldest| now - oldest <= STEADY_WINDOW)
        {
            Verdict::Steady
        } else if now - first >= LONGEST {
            Verdict::Timeout
        } else {
            Verdict::Hold
        }
    }
}

/// Start the splash a package declares. Called once as d3d12_xs.dll attaches,
/// on the loading thread before the title's entry point, so the hook exists
/// before the title creates its window.
pub(crate) fn install() {
    let Some(image) = std::env::current_exe().ok().and_then(|exe| {
        let config = std::fs::read_to_string(exe.with_file_name("MicrosoftGame.Config")).ok()?;
        Some(exe.parent()?.join(declaration(&config)?))
    }) else {
        return;
    };
    let splash = SPLASH.get_or_init(|| Splash {
        state: Mutex::new(State { stage: Stage::Waiting, title: None }),
        changed: Condvar::new(),
        painting: Mutex::default(),
        pacing: Mutex::default(),
    });
    // In-context, the callback runs on the thread that creates each window.
    // The hook names this library, which holds the callback.
    let mut module = HMODULE::default();
    let hook = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR((on_event as *const ()).cast()),
            &mut module,
        )
        .map(|()| {
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_CREATE,
                Some(module),
                Some(on_event),
                GetCurrentProcessId(),
                0,
                WINEVENT_INCONTEXT,
            )
        })
        .unwrap_or_default()
    };
    // The thread starts once the loader lock is free, before the title runs.
    let started = !hook.is_invalid()
        && std::thread::Builder::new()
            .name("TuxBox splash".into())
            .spawn(move || run(splash, &image))
            .is_ok();
    if !started {
        lock(&splash.state).stage = Stage::Released;
    }
}

unsafe extern "system" fn on_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if event != EVENT_OBJECT_CREATE || object != OBJID_WINDOW.0 || child != CHILDID_SELF as i32 {
        return;
    }
    let Some(splash) = SPLASH.get() else { return };
    {
        let mut state = lock(&splash.state);
        if state.stage != Stage::Waiting || !unsafe { is_title_window(hwnd) } {
            return;
        }
        state.stage = Stage::Fitting;
    }
    // This is the window's own thread: the size applies before the title
    // shows the window.
    let _ = window::fit_created_window(hwnd, CONSOLE_OUTPUT.0, CONSOLE_OUTPUT.1);
    let mut state = lock(&splash.state);
    state.title = Some(hwnd.0 as usize);
    // The title may have presented meanwhile.
    if state.stage == Stage::Fitting {
        state.stage = Stage::Claimed;
    }
    splash.changed.notify_all();
}

/// A top-level window of the title's own: not a child, owned, message-only,
/// tool or IME window, not one of TuxBox's, and not a small helper.
unsafe fn is_title_window(hwnd: HWND) -> bool {
    unsafe {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let extended = WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32);
        let instance = GetWindowLongPtrW(hwnd, GWLP_HINSTANCE);
        let exe = GetModuleHandleW(None).map_or(0, |m| m.0 as isize);
        let mut name = [0u16; 64];
        let length = GetClassNameW(hwnd, &mut name).max(0) as usize;
        let class = String::from_utf16_lossy(&name[..length]);
        let mut bounds = RECT::default();
        GetAncestor(hwnd, GA_PARENT) == GetDesktopWindow()
            && !style.contains(WS_CHILD)
            && GetWindow(hwnd, GW_OWNER).is_err()
            && !extended.contains(WS_EX_TOOLWINDOW)
            && (instance == 0 || instance == exe)
            && class != "IME"
            && !class.starts_with("TuxBox.")
            && GetWindowRect(hwnd, &mut bounds).is_ok()
            && bounds.right - bounds.left >= 320
            && bounds.bottom - bounds.top >= 200
    }
}

fn run(splash: &Splash, image: &Path) {
    let art = unsafe {
        let com = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let art = decode(image);
        if com {
            CoUninitialize();
        }
        art
    };
    let art = match art {
        Ok(art) => art,
        Err(error) => {
            crate::diagnostics::log(&format!(
                "package splash {} not shown: {:#010x}",
                image.display(),
                error.code().0
            ));
            lock(&splash.state).stage = Stage::Released;
            return;
        }
    };
    // The image at the size last painted, if it had to be scaled.
    let mut scaled: Option<((u32, u32), Vec<u8>)> = None;
    loop {
        {
            let state = lock(&splash.state);
            if state.stage == Stage::Released {
                return;
            }
            let _ = splash.changed.wait_timeout(state, REPAINT);
        }
        let _painting = lock(&splash.painting);
        let (title, first) = {
            let state = lock(&splash.state);
            match (state.stage, state.title) {
                (Stage::Claimed, Some(title)) => (HWND(title as _), true),
                (Stage::Shown, Some(title)) => (HWND(title as _), false),
                (Stage::Released, _) => return,
                _ => continue,
            }
        };
        if !unsafe { IsWindow(Some(title)) }.as_bool() {
            lock(&splash.state).stage = Stage::Released;
            return;
        }
        let Some(size) = (unsafe { paint(title, &art, &mut scaled) }) else {
            continue;
        };
        if first {
            let mut state = lock(&splash.state);
            if state.stage == Stage::Claimed {
                state.stage = Stage::Shown;
                crate::diagnostics::log(&format!(
                    "showing package splash {} ({}x{}) at {}x{}",
                    image.file_name().unwrap_or_default().to_string_lossy(),
                    art.width,
                    art.height,
                    size.0,
                    size.1
                ));
            }
        }
    }
}

/// Paint the image into the window's client area, over black bars where the
/// proportions differ. Returns the
/// size painted, or `None` while the window has no visible area.
unsafe fn paint(
    window: HWND,
    art: &Art,
    scaled: &mut Option<((u32, u32), Vec<u8>)>,
) -> Option<(u32, u32)> {
    unsafe {
        let mut client = RECT::default();
        if !IsWindowVisible(window).as_bool() || GetClientRect(window, &mut client).is_err() {
            return None;
        }
        let at = placement((client.right, client.bottom), (art.width, art.height));
        let size = ((at.right - at.left) as u32, (at.bottom - at.top) as u32);
        if size.0 == 0 || size.1 == 0 {
            return None;
        }
        let pixels = if size == (art.width, art.height) {
            &art.pixels
        } else {
            if scaled.as_ref().is_none_or(|(made, _)| *made != size) {
                *scaled = Some((size, resize(&art.pixels, (art.width, art.height), size)?));
            }
            &scaled.as_ref()?.1
        };
        // Premultiplied colours drawn without their alpha are the image over black.
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.0 as i32,
                biHeight: -(size.1 as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = GetDC(Some(window));
        if target.is_invalid() {
            return None;
        }
        let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
        let borders = [
            RECT { left: 0, top: 0, right: client.right, bottom: at.top },
            RECT { left: 0, top: at.bottom, right: client.right, bottom: client.bottom },
            RECT { left: 0, top: at.top, right: at.left, bottom: at.bottom },
            RECT { left: at.right, top: at.top, right: client.right, bottom: at.bottom },
        ];
        for border in &borders {
            FillRect(target, border, black);
        }
        let lines = SetDIBitsToDevice(
            target,
            at.left,
            at.top,
            size.0,
            size.1,
            0,
            0,
            0,
            size.1,
            pixels.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(Some(window), target);
        // Without this Wine shows the drawing only when the window's thread is idle.
        let _ = GdiFlush();
        (lines != 0).then_some(size)
    }
}

fn decode(path: &Path) -> Result<Art> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: COM is initialized on this thread; the path stays alive for the call.
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let decoder = factory.CreateDecoderFromFilename(
            PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let converter = factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut width, mut height) = (0, 0);
        converter.GetSize(&mut width, &mut height)?;
        if width == 0 || height == 0 || width > LARGEST_IMAGE || height > LARGEST_IMAGE {
            return Err(E_INVALIDARG.into());
        }
        let bytes = width as usize * height as usize * 4;
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(bytes).map_err(|_| E_OUTOFMEMORY)?;
        pixels.resize(bytes, 0);
        converter.CopyPixels(core::ptr::null(), width * 4, &mut pixels)?;
        Ok(Art { width, height, pixels })
    }
}

/// Whether a title frame presented before the swap chain exists stays behind
/// the splash. Once this returns false it keeps doing so, and nothing paints
/// the title's window any more.
pub(super) fn hold() -> bool {
    let Some(splash) = SPLASH.get() else {
        return false;
    };
    let (shown, verdict) = {
        let mut state = lock(&splash.state);
        let verdict = match state.stage {
            Stage::Released => return false,
            Stage::Shown => lock(&splash.pacing).arrive(Instant::now()),
            // The title presents without a splash up: nothing to hold for.
            _ => Verdict::Steady,
        };
        if verdict == Verdict::Hold {
            return true;
        }
        let shown = state.stage == Stage::Shown;
        state.stage = Stage::Released;
        splash.changed.notify_all();
        (shown, verdict)
    };
    // A swap chain follows: wait out a paint under way.
    drop(lock(&splash.painting));
    let pacing = lock(&splash.pacing);
    crate::diagnostics::log(&match (shown, verdict) {
        (true, Verdict::Steady) => format!(
            "package splash ends at title frame {}, {:.1}s after the first: frames came steadily",
            pacing.frames,
            pacing.first.map_or(0.0, |first| first.elapsed().as_secs_f32())
        ),
        (true, _) => format!(
            "package splash ends at title frame {}: frames did not come steadily within {}s",
            pacing.frames,
            LONGEST.as_secs()
        ),
        _ => "package splash not shown: the title presented before it was painted".into(),
    });
    false
}

#[cfg(test)]
#[path = "../tests/present/splash.rs"]
mod tests;
