//! Native Win32 dot shown next to a text selection.
//!
//! The dot is a layered, topmost, non-activating popup window. It is created
//! once on a dedicated thread and reused via ShowWindow/SetWindowPos. A
//! low-level mouse hook watches selections and clicks; a click reads the live
//! selection + paragraph from the target HWND and forwards them to Electron
//! through a napi ThreadsafeFunction.
//!
//! Interaction spec: docs/dot-trigger-spec.md

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread;
use std::time::Duration;

use napi::threadsafe_function::{
  ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode,
};
use napi_derive::napi;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
  COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
  CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
  MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
  BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HDC, MONITORINFO,
  MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::{
  GetModuleHandleExW, GetModuleHandleW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
  GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::UI::WindowsAndMessaging::{
  CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
  GetForegroundWindow, GetMessageW, GetWindowRect, GetWindowThreadProcessId, KillTimer,
  LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SetCursor, SetTimer,
  SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx,
  UpdateLayeredWindow, MSG, MSLLHOOKSTRUCT, WNDCLASSW, WNDCLASS_STYLES, WM_CLOSE, WM_DESTROY,
  WM_ERASEBKGND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEACTIVATE, WM_MOUSEMOVE,
  WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_SETCURSOR, WM_TIMER, HWND_TOPMOST, IDC_HAND,
  MA_NOACTIVATE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WH_MOUSE_LL,
  WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::{probe_expand_for_window_sync, probe_selection_for_window_sync, SelectionRect};

const DOT_WINDOW_SIZE: i32 = 24;
const DOT_VISUAL_SIZE: f64 = 8.0;
const DOT_MARGIN: i32 = 8;
const TIMER_ID: usize = 1;
const CLASS_NAME: PCWSTR = w!("UiaDotWindow");

static DOT_HWND: AtomicIsize = AtomicIsize::new(0);
static DOT_STARTED: AtomicBool = AtomicBool::new(false);

#[napi(object)]
pub struct DotSelectionEvent {
  pub text: String,
  pub rects: Vec<SelectionRect>,
  pub process_name: String,
  pub paragraph: Option<String>,
  pub paragraph_error: Option<String>,
  /// Dot window rectangle in absolute physical pixels.
  pub dot_rect: SelectionRect,
}

struct DotState {
  hwnd: HWND,
  hook: windows::Win32::UI::WindowsAndMessaging::HHOOK,
  tsfn: ThreadsafeFunction<DotSelectionEvent, ErrorStrategy::Fatal>,
  visible: bool,
  dot_rect: RECT,
  target_hwnd: HWND,
  suppress_mouse_up: bool,
  /// 200ms timer tick counter; every third tick re-checks the live selection.
  ticks: u32,
}

thread_local! {
  static DOT: RefCell<Option<DotState>> = const { RefCell::new(None) };
}

/// Start the native dot thread. Safe to call more than once; only the first
/// call starts a thread. The callback receives one event per dot click.
#[napi]
pub fn start_dot(
  callback: ThreadsafeFunction<DotSelectionEvent, ErrorStrategy::Fatal>,
) -> napi::Result<()> {
  if DOT_STARTED.swap(true, Ordering::SeqCst) {
    return Ok(());
  }
  let spawned = thread::Builder::new()
    .name("uia-dot".into())
    .spawn(move || {
      if let Err(error) = run_dot_thread(callback) {
        eprintln!("[uia-dot] thread exited: {error}");
      }
      DOT_STARTED.store(false, Ordering::SeqCst);
    });
  if let Err(error) = spawned {
    DOT_STARTED.store(false, Ordering::SeqCst);
    return Err(napi::Error::from_reason(format!(
      "failed to spawn uia-dot thread: {error}"
    )));
  }
  Ok(())
}

/// Stop the dot thread. Used on app quit.
#[napi]
pub fn stop_dot() {
  let raw = DOT_HWND.load(Ordering::SeqCst);
  if raw != 0 {
    unsafe {
      let _ = PostMessageW(
        HWND(raw as *mut core::ffi::c_void),
        WM_CLOSE,
        WPARAM(0),
        LPARAM(0),
      );
    }
  }
}

fn run_dot_thread(
  tsfn: ThreadsafeFunction<DotSelectionEvent, ErrorStrategy::Fatal>,
) -> windows::core::Result<()> {
  unsafe {
    let hinstance = HINSTANCE(GetModuleHandleW(PCWSTR::null())?.0);
    let cursor = LoadCursorW(None, IDC_HAND).unwrap_or_default();
    let wc = WNDCLASSW {
      style: WNDCLASS_STYLES(0),
      lpfnWndProc: Some(wnd_proc),
      hInstance: hinstance,
      hCursor: cursor,
      lpszClassName: CLASS_NAME,
      ..Default::default()
    };
    let _ = RegisterClassW(&wc);

    let hwnd = CreateWindowExW(
      WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
      CLASS_NAME,
      w!(""),
      WS_POPUP,
      0,
      0,
      DOT_WINDOW_SIZE,
      DOT_WINDOW_SIZE,
      None,
      None,
      hinstance,
      None,
    )?;
    DOT_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    update_dot_surface(hwnd)?;

    let module = hook_module()?;
    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), module, 0)?;
    DOT.with(|slot| {
      *slot.borrow_mut() = Some(DotState {
        hwnd,
        hook,
        tsfn,
        visible: false,
        dot_rect: RECT::default(),
        target_hwnd: HWND::default(),
        suppress_mouse_up: false,
        ticks: 0,
      });
    });
    SetTimer(hwnd, TIMER_ID, 200, None);

    let mut message = MSG::default();
    while GetMessageW(&mut message, HWND::default(), 0, 0).0 > 0 {
      let _ = TranslateMessage(&message);
      DispatchMessageW(&message);
    }

    let _ = KillTimer(hwnd, TIMER_ID);
    let hook = DOT.with(|slot| slot.borrow_mut().take().map(|state| state.hook));
    if let Some(hook) = hook {
      let _ = UnhookWindowsHookEx(hook);
    }
    let _ = DestroyWindow(hwnd);
    DOT_HWND.store(0, Ordering::SeqCst);
  }
  Ok(())
}

unsafe fn hook_module() -> windows::core::Result<HINSTANCE> {
  let mut module = windows::Win32::Foundation::HMODULE::default();
  GetModuleHandleExW(
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    PCWSTR(mouse_hook_proc as *const u16),
    &mut module,
  )?;
  Ok(HINSTANCE(module.0))
}

unsafe extern "system" fn wnd_proc(
  hwnd: HWND,
  message: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match message {
    WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
    WM_SETCURSOR | WM_MOUSEMOVE => {
      let cursor = LoadCursorW(None, IDC_HAND).unwrap_or_default();
      let _ = SetCursor(cursor);
      LRESULT(1)
    }
    WM_LBUTTONDOWN => {
      handle_dot_click();
      LRESULT(0)
    }
    WM_TIMER => {
      check_visibility();
      LRESULT(0)
    }
    WM_ERASEBKGND => LRESULT(1),
    WM_CLOSE => {
      let _ = DestroyWindow(hwnd);
      LRESULT(0)
    }
    WM_DESTROY => {
      PostQuitMessage(0);
      LRESULT(0)
    }
    _ => DefWindowProcW(hwnd, message, wparam, lparam),
  }
}

unsafe extern "system" fn mouse_hook_proc(
  code: i32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  if code >= 0 {
    let message = wparam.0 as u32;
    match message {
      WM_LBUTTONDOWN => {
        let point = mouse_point(lparam);
        DOT.with(|slot| {
          if let Some(state) = slot.borrow_mut().as_mut() {
            if state.visible && point_in_rect(point, state.dot_rect) {
              state.suppress_mouse_up = true;
            } else {
              hide_dot_state(state);
            }
          }
        });
      }
      WM_LBUTTONUP => {
        let suppress = DOT.with(|slot| {
          slot
            .borrow_mut()
            .as_mut()
            .map(|state| {
              let value = state.suppress_mouse_up;
              state.suppress_mouse_up = false;
              value
            })
            .unwrap_or(false)
        });
        if !suppress {
          handle_mouse_up();
        }
      }
      WM_MOUSEWHEEL | WM_MBUTTONDOWN | WM_RBUTTONDOWN => {
        DOT.with(|slot| {
          if let Some(state) = slot.borrow_mut().as_mut() {
            hide_dot_state(state);
          }
        });
      }
      _ => {}
    }
  }
  CallNextHookEx(None, code, wparam, lparam)
}

unsafe fn mouse_point(lparam: LPARAM) -> POINT {
  let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
  info.pt
}

fn point_in_rect(point: POINT, rect: RECT) -> bool {
  point.x >= rect.left
    && point.x < rect.right
    && point.y >= rect.top
    && point.y < rect.bottom
}

/// Mouse-up: read the target window and show the dot when the selection is
/// valid English text in a non-password field.
unsafe fn handle_mouse_up() {
  let target = GetForegroundWindow();
  if target.0.is_null() {
    hide_dot();
    return;
  }
  if is_own_process(target) || is_fullscreen(target) {
    hide_dot();
    return;
  }

  let selection = probe_selection_for_window_sync(
    target.0 as isize,
    Duration::from_millis(150),
  );
  if selection.error_code.is_some()
    || selection.text.trim().is_empty()
    || !english_ratio_ok(&selection.text)
  {
    hide_dot();
    return;
  }

  let Some(rect) = selection
    .rects
    .iter()
    .rev()
    .find(|rect| rect.width > 1.0)
    .or_else(|| selection.rects.last())
  else {
    hide_dot();
    return;
  };
  show_dot(rect, target);
}

/// Click on the dot: read the live selection + paragraph from the target
/// window captured at mouse-up, hide the dot, and forward the event.
unsafe fn handle_dot_click() {
  let (target, dot_rect) = DOT.with(|slot| {
    slot
      .borrow_mut()
      .as_mut()
      .map(|state| {
        let target = state.target_hwnd;
        let dot_rect = state.dot_rect;
        hide_dot_state(state);
        (target, dot_rect)
      })
      .unwrap_or((HWND::default(), RECT::default()))
  });
  if target.0.is_null() {
    return;
  }

  let selection =
    probe_selection_for_window_sync(target.0 as isize, Duration::from_millis(200));
  if selection.error_code.is_some()
    || selection.text.trim().is_empty()
    || !english_ratio_ok(&selection.text)
  {
    return;
  }

  let expand = probe_expand_for_window_sync(target.0 as isize, Duration::from_millis(300));
  let (paragraph, paragraph_error) = if expand.success {
    let text = expand
      .results
      .iter()
      .find(|result| result.mode == "selection" && result.unit == "Paragraph")
      .map(|result| {
        result
          .text
          .trim()
          .chars()
          .take(2000)
          .collect::<String>()
      })
      .filter(|text| !text.is_empty());
    let has_text = text.is_some();
    (text, if has_text { None } else { Some("paragraph unit missing".into()) })
  } else {
    (
      None,
      Some(
        expand
          .error_message
          .unwrap_or_else(|| "paragraph probe failed".into()),
      ),
    )
  };

  let event = DotSelectionEvent {
    text: selection.text,
    rects: selection.rects,
    process_name: selection.process_name,
    paragraph,
    paragraph_error,
    dot_rect: rect_to_selection_rect(dot_rect),
  };

  DOT.with(|slot| {
    if let Some(state) = slot.borrow().as_ref() {
      let _ = state
        .tsfn
        .call(event, ThreadsafeFunctionCallMode::Blocking);
    }
  });
}

unsafe fn show_dot(rect: &SelectionRect, target: HWND) {
  let mut x = rect.x as i32 + rect.width as i32 + DOT_MARGIN;
  let mut y = rect.y as i32 + rect.height as i32 + DOT_MARGIN;

  let monitor = MonitorFromPoint(
    POINT {
      x: rect.x as i32,
      y: rect.y as i32,
    },
    MONITOR_DEFAULTTONEAREST,
  );
  let mut info = MONITORINFO {
    cbSize: core::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  if GetMonitorInfoW(monitor, &mut info).as_bool() {
    let work = info.rcWork;
    if x + DOT_WINDOW_SIZE > work.right {
      x = rect.x as i32 - DOT_MARGIN - DOT_WINDOW_SIZE;
    }
    if y + DOT_WINDOW_SIZE > work.bottom {
      y = rect.y as i32 - DOT_MARGIN - DOT_WINDOW_SIZE;
    }
    x = x.max(work.left).min(work.right - DOT_WINDOW_SIZE);
    y = y.max(work.top).min(work.bottom - DOT_WINDOW_SIZE);
  }

  DOT.with(|slot| {
    if let Some(state) = slot.borrow_mut().as_mut() {
      state.dot_rect = RECT {
        left: x,
        top: y,
        right: x + DOT_WINDOW_SIZE,
        bottom: y + DOT_WINDOW_SIZE,
      };
      state.target_hwnd = target;
      state.visible = true;
      state.suppress_mouse_up = false;
      state.ticks = 0;
      let _ = SetWindowPos(
        state.hwnd,
        HWND_TOPMOST,
        x,
        y,
        DOT_WINDOW_SIZE,
        DOT_WINDOW_SIZE,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
      );
      let _ = ShowWindow(state.hwnd, SW_SHOWNOACTIVATE);
    }
  });
}

unsafe fn hide_dot() {
  DOT.with(|slot| {
    if let Some(state) = slot.borrow_mut().as_mut() {
      hide_dot_state(state);
    }
  });
}

unsafe fn hide_dot_state(state: &mut DotState) {
  if state.visible {
    let _ = ShowWindow(state.hwnd, SW_HIDE);
    state.visible = false;
  }
  state.ticks = 0;
}

unsafe fn check_visibility() {
  let (visible, target, tick) = DOT.with(|slot| {
    slot
      .borrow()
      .as_ref()
      .map(|state| (state.visible, state.target_hwnd, state.ticks))
      .unwrap_or((false, HWND::default(), 0))
  });
  if !visible {
    return;
  }
  let foreground = GetForegroundWindow();
  if foreground.0.is_null()
    || foreground != target
    || is_own_process(foreground)
    || is_fullscreen(foreground)
  {
    hide_dot();
    return;
  }

  let next_tick = tick.wrapping_add(1);
  DOT.with(|slot| {
    if let Some(state) = slot.borrow_mut().as_mut() {
      state.ticks = next_tick;
    }
  });
  if next_tick % 3 != 0 {
    return;
  }

  // Prototype only: selection-cleared detection via a low-frequency re-read.
  // The product should use UIA_Text_TextSelectionChangedEventId instead.
  let selection =
    probe_selection_for_window_sync(target.0 as isize, Duration::from_millis(100));
  if selection.error_code.is_some() || selection.text.trim().is_empty() {
    hide_dot();
  }
}

unsafe fn is_own_process(hwnd: HWND) -> bool {
  let mut pid = 0u32;
  GetWindowThreadProcessId(hwnd, Some(&mut pid));
  pid == std::process::id()
}

unsafe fn is_fullscreen(hwnd: HWND) -> bool {
  let mut rect = RECT::default();
  if GetWindowRect(hwnd, &mut rect).is_err() {
    return false;
  }
  let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
  let mut info = MONITORINFO {
    cbSize: core::mem::size_of::<MONITORINFO>() as u32,
    ..Default::default()
  };
  if !GetMonitorInfoW(monitor, &mut info).as_bool() {
    return false;
  }
  let monitor = info.rcMonitor;
  rect.left <= monitor.left
    && rect.top <= monitor.top
    && rect.right >= monitor.right
    && rect.bottom >= monitor.bottom
}

fn english_ratio_ok(text: &str) -> bool {
  let mut letters = 0usize;
  let mut total = 0usize;
  for ch in text.chars() {
    if ch.is_whitespace() {
      continue;
    }
    total += 1;
    if ch.is_ascii_alphabetic() {
      letters += 1;
    }
  }
  total > 0 && (letters as f64 / total as f64) >= 0.30
}

fn rect_to_selection_rect(rect: RECT) -> SelectionRect {
  SelectionRect {
    x: rect.left as f64,
    y: rect.top as f64,
    width: (rect.right - rect.left) as f64,
    height: (rect.bottom - rect.top) as f64,
  }
}

unsafe fn update_dot_surface(hwnd: HWND) -> windows::core::Result<()> {
  let screen_dc: HDC = GetDC(HWND::default());
  let memory_dc = CreateCompatibleDC(screen_dc);
  let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
  let bitmap_info = BITMAPINFO {
    bmiHeader: BITMAPINFOHEADER {
      biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
      biWidth: DOT_WINDOW_SIZE,
      biHeight: -DOT_WINDOW_SIZE,
      biPlanes: 1,
      biBitCount: 32,
      biCompression: BI_RGB.0,
      ..Default::default()
    },
    ..Default::default()
  };
  let bitmap = CreateDIBSection(
    memory_dc,
    &bitmap_info,
    DIB_RGB_COLORS,
    &mut bits,
    None,
    0,
  )?;
  let previous = SelectObject(memory_dc, bitmap);
  let pixels = std::slice::from_raw_parts_mut(
    bits.cast::<u32>(),
    (DOT_WINDOW_SIZE * DOT_WINDOW_SIZE) as usize,
  );
  draw_dot(pixels);

  let size = SIZE {
    cx: DOT_WINDOW_SIZE,
    cy: DOT_WINDOW_SIZE,
  };
  let source = POINT { x: 0, y: 0 };
  let blend = BLENDFUNCTION {
    BlendOp: AC_SRC_OVER as u8,
    BlendFlags: 0,
    SourceConstantAlpha: 255,
    AlphaFormat: AC_SRC_ALPHA as u8,
  };
  UpdateLayeredWindow(
    hwnd,
    screen_dc,
    None,
    Some(&size),
    memory_dc,
    Some(&source),
    COLORREF(0),
    Some(&blend),
    windows::Win32::UI::WindowsAndMessaging::ULW_ALPHA,
  )?;

  SelectObject(memory_dc, previous);
  let _ = DeleteObject(bitmap);
  let _ = DeleteDC(memory_dc);
  ReleaseDC(HWND::default(), screen_dc);
  Ok(())
}

/// Draw an 8px black dot with a 1px white ring, antialiased with 4x4
/// supersampling. Pixels are premultiplied BGRA as required by
/// UpdateLayeredWindow + AC_SRC_ALPHA.
fn draw_dot(pixels: &mut [u32]) {
  let center = DOT_WINDOW_SIZE as f64 / 2.0;
  let radius = DOT_VISUAL_SIZE / 2.0;
  let ring = radius + 1.0;
  const SAMPLES: i32 = 4;

  for y in 0..DOT_WINDOW_SIZE {
    for x in 0..DOT_WINDOW_SIZE {
      let mut alpha_sum = 0u32;
      let mut color_sum = 0u32;
      for sy in 0..SAMPLES {
        for sx in 0..SAMPLES {
          let px = x as f64 + (sx as f64 + 0.5) / SAMPLES as f64;
          let py = y as f64 + (sy as f64 + 0.5) / SAMPLES as f64;
          let distance = ((px - center).powi(2) + (py - center).powi(2)).sqrt();
          if distance <= radius {
            alpha_sum += 255;
            // black core: premultiplied RGB stays 0
          } else if distance <= ring {
            alpha_sum += 255;
            color_sum += 255;
          }
        }
      }
      let samples = (SAMPLES * SAMPLES) as u32;
      let alpha = alpha_sum / samples;
      let white = color_sum / samples;
      let index = (y * DOT_WINDOW_SIZE + x) as usize;
      pixels[index] = (alpha << 24) | (white << 16) | (white << 8) | white;
    }
  }
}
