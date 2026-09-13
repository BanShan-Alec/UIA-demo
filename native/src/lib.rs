//! uia-probe: Windows UI Automation text-selection extraction for Chromium browsers.
//!
//! Design notes:
//! - All UIA COM calls are synchronous and cross-process (they can block if the
//!   target app hangs). Every probe therefore runs on a dedicated COM worker
//!   thread; the JS-facing `probeSelection` is a `napi::Task` (libuv worker)
//!   that waits on a channel with a hard timeout (default 500 ms).
//! - If a probe exceeds the timeout the worker is abandoned and a fresh worker
//!   is spawned on the next call, so a hung target can never wedge the addon.
//! - Zero clipboard usage. The probe path contains no clipboard API calls at
//!   all — UIA reads the selection out-of-process, so the clipboard cannot be
//!   affected by design.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Mutex, OnceLock, atomic::{AtomicU64, Ordering}};
use std::thread;
use std::time::{Duration, Instant};

use napi::bindgen_prelude::*;
use napi::{Env, Task};
use napi_derive::napi;

use windows::core::{Interface, PWSTR, VARIANT};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Com::{
  CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
  SAFEARRAY,
};
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
use windows::Win32::System::Threading::{
  OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{
  IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextRange,
  IUIAutomationTextRangeArray, CUIAutomation, TreeScope_Descendants, UIA_DocumentControlTypeId,
  UIA_EditControlTypeId, UIA_IsTextPatternAvailablePropertyId, UIA_TextPatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
  GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
};

const MAX_TEXT_CHARS: usize = 10_000;
const MAX_RECTS: usize = 4_096;

// ---------------------------------------------------------------------------
// napi types
// ---------------------------------------------------------------------------

#[napi(object)]
pub struct SelectionRect {
  /// Absolute screen coordinate (physical pixels) of the line fragment.
  pub x: f64,
  pub y: f64,
  pub width: f64,
  pub height: f64,
}

#[napi(object)]
pub struct FocusInfo {
  pub name: String,
  pub control_type: String,
  /// Which strategy located the text pattern: "focused-element" or "tree-search".
  pub via: String,
  pub process_id: u32,
}

#[napi(object)]
pub struct ProbeResult {
  pub success: bool,
  /// Executable name of the foreground window, e.g. "msedge.exe".
  pub process_name: String,
  pub process_id: u32,
  pub window_title: String,
  pub focused_element: Option<FocusInfo>,
  pub text: Option<String>,
  pub rect_count: u32,
  pub rects: Vec<SelectionRect>,
  /// Time spent inside the native probe (ms), excludes JS/Electron overhead.
  pub native_elapsed_ms: f64,
  pub error_code: Option<String>,
  pub error_message: Option<String>,
  pub warnings: Vec<String>,
}

impl ProbeResult {
  fn err(code: &str, msg: impl Into<String>, start: Instant) -> Self {
    ProbeResult {
      success: false,
      process_name: String::new(),
      process_id: 0,
      window_title: String::new(),
      focused_element: None,
      text: None,
      rect_count: 0,
      rects: Vec::new(),
      native_elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
      error_code: Some(code.to_string()),
      error_message: Some(msg.into()),
      warnings: Vec::new(),
    }
  }
}

/// Product-facing selection result for a word-selection translation app.
/// Four fields only: what to translate, where to place the popup, what went
/// wrong, and which app the selection came from.
#[napi(object)]
pub struct SelectionResult {
  /// Selected text. Empty string when no valid selection.
  pub text: String,
  /// Per-line bounding boxes in absolute physical pixels. Empty on failure.
  pub rects: Vec<SelectionRect>,
  /// None = success. Otherwise `EmptySelection` / `TextPatternNotSupported` /
  /// `Timeout` / `ElementNotFound` / `ComError` / `NoForegroundWindow`.
  pub error_code: Option<String>,
  /// Executable name of the foreground app (e.g. "msedge.exe") — for
  /// enable/disable rules per application.
  pub process_name: String,
}

impl From<ProbeResult> for SelectionResult {
  fn from(r: ProbeResult) -> Self {
    SelectionResult {
      text: if r.success { r.text.unwrap_or_default() } else { String::new() },
      rects: if r.success { r.rects } else { Vec::new() },
      error_code: if r.success { None } else { r.error_code },
      process_name: r.process_name,
    }
  }
}

// ---------------------------------------------------------------------------
// dedicated UIA worker thread (COM MTA)
// ---------------------------------------------------------------------------

enum ProbeCmd {
  Probe { reply: Sender<ProbeResult> },
}

struct WorkerHandle {
  tx: Sender<ProbeCmd>,
  id: u64,
}

static WORKER: OnceLock<Mutex<Option<WorkerHandle>>> = OnceLock::new();
static WORKER_SEQ: AtomicU64 = AtomicU64::new(0);

fn worker_slot() -> &'static Mutex<Option<WorkerHandle>> {
  WORKER.get_or_init(|| Mutex::new(None))
}

/// Run one UIA probe with a hard timeout. Called from `ProbeTask::compute`
/// (libuv thread); the actual COM work happens on the worker thread.
fn probe_with_timeout(timeout: Duration) -> ProbeResult {
  let start = Instant::now();
  let (reply_tx, reply_rx) = channel::<ProbeResult>();

  {
    let mut guard = worker_slot().lock().unwrap_or_else(|e| e.into_inner());
    // The worker may have died (stuck probe abandoned earlier); retry once
    // with a freshly spawned worker.
    let mut sent = guard
      .as_ref()
      .map(|w| w.tx.send(ProbeCmd::Probe { reply: reply_tx.clone() }).is_ok())
      .unwrap_or(false);
    if !sent {
      *guard = spawn_worker();
      sent = guard
        .as_ref()
        .map(|w| w.tx.send(ProbeCmd::Probe { reply: reply_tx.clone() }).is_ok())
        .unwrap_or(false);
    }
    if !sent {
      return ProbeResult::err(
        "ComInitFailed",
        "failed to start UIA worker thread",
        start,
      );
    }
  }

  match reply_rx.recv_timeout(timeout) {
    Ok(result) => result,
    Err(RecvTimeoutError::Timeout) => {
      // Abandon the stuck worker; the next probe spawns a fresh one. The old
      // thread exits on its own once its channel is disconnected.
      let mut guard = worker_slot().lock().unwrap_or_else(|e| e.into_inner());
      *guard = None;
      ProbeResult::err(
        "Timeout",
        format!(
          "UIA probe did not complete within {} ms (target app may be unresponsive)",
          timeout.as_millis()
        ),
        start,
      )
    }
    Err(RecvTimeoutError::Disconnected) => {
      ProbeResult::err(
        "ComError",
        "UIA worker thread exited unexpectedly",
        start,
      )
    }
  }
}

fn spawn_worker() -> Option<WorkerHandle> {
  let (tx, rx) = channel::<ProbeCmd>();
  let id = WORKER_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
  let spawned = thread::Builder::new()
    .name(format!("uia-worker-{id}"))
    .spawn(move || {
      // COM MTA for the lifetime of this thread (recommended mode for UIA
      // clients: avoids re-entrancy deadlocks in STA callbacks).
      let com_init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
      while let Ok(cmd) = rx.recv() {
        let ProbeCmd::Probe { reply } = cmd;
        let result = if com_init.is_ok() {
          unsafe { run_probe() }
        } else {
          ProbeResult::err(
            "ComInitFailed",
            format!("CoInitializeEx failed: {com_init:?}"),
            Instant::now(),
          )
        };
        let _ = reply.send(result); // receiver may be gone after a timeout — fine
      }
      unsafe {
        windows::Win32::System::Com::CoUninitialize();
      }
    });
  spawned.ok().map(|_| WorkerHandle { tx, id })
}

// ---------------------------------------------------------------------------
// the actual probe
// ---------------------------------------------------------------------------

unsafe fn run_probe() -> ProbeResult {
  let start = Instant::now();

  macro_rules! fail {
    ($code:expr, $($arg:tt)*) => {
      return ProbeResult::err($code, format!($($arg)*), start)
    };
  }

  // Same as `fail!` but carries the foreground-window context that was
  // already collected (used for failures after the window was identified).
  // A closure (not a macro) so the captured variables resolve normally.
  let fail_ctx = |mut result: ProbeResult, code: &str, msg: String| -> ProbeResult {
    result.success = false;
    result.error_code = Some(code.to_string());
    result.error_message = Some(msg);
    result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result
  };

  // --- foreground window -------------------------------------------------
  let hwnd: HWND = GetForegroundWindow();
  if hwnd.0.is_null() {
    fail!("NoForegroundWindow", "GetForegroundWindow returned NULL");
  }
  let window_title = window_text(hwnd);
  let pid = window_pid(hwnd);
  let process_name = process_image_name(pid);

  let mut result = ProbeResult {
    success: false,
    process_name: process_name.clone(),
    process_id: pid,
    window_title: window_title.clone(),
    focused_element: None,
    text: None,
    rect_count: 0,
    rects: Vec::new(),
    native_elapsed_ms: 0.0,
    error_code: None,
    error_message: None,
    warnings: Vec::new(),
  };

  // --- UIA client --------------------------------------------------------
  let automation: IUIAutomation =
    match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
      Ok(a) => a,
      Err(e) => fail!("ComError", "CoCreateInstance(CLSID_CUIAutomation) failed: {e}"),
    };

  // Primary strategy: the focused element. In Chromium the document (or the
  // focused text field) carries the TextPattern and reflects the live
  // selection. This is the fast path for hot calls.
  let focused: IUIAutomationElement = match automation.GetFocusedElement() {
    Ok(el) => el,
    Err(e) => fail!("ElementNotFound", "GetFocusedElement failed: {e}"),
  };

  let mut focus_info = FocusInfo {
    name: truncate(el_name(&focused), 120),
    control_type: control_type_name(&focused),
    via: "focused-element".into(),
    process_id: el_pid(&focused),
  };

  let mut pattern = text_pattern(&focused);

  // Fallback strategy: search the foreground window subtree for the first
  // element exposing TextPattern (e.g. the web-area document when focus is
  // reported on a wrapper node).
  if pattern.is_none() {
    focus_info.via = "tree-search".into();
    let flag = VARIANT::from(true);
    let condition = match automation.CreatePropertyCondition(UIA_IsTextPatternAvailablePropertyId, &flag) {
      Ok(c) => c,
      Err(e) => fail!("ComError", "CreatePropertyCondition failed: {e}"),
    };
    let root: IUIAutomationElement = match automation.ElementFromHandle(hwnd) {
      Ok(r) => r,
      Err(e) => {
        return fail_ctx(result, "ElementNotFound", format!("ElementFromHandle(foreground) failed: {e}"))
      }
    };
    match root.FindFirst(TreeScope_Descendants, &condition) {
      Ok(el) => pattern = text_pattern(&el),
      Err(_) => {
        return fail_ctx(
          result,
          "TextPatternNotSupported",
          "no element exposing TextPattern found in foreground window (compatibility boundary, e.g. Chromium PDF viewer)".into(),
        )
      }
    }
  }
  result.focused_element = Some(focus_info);

  let pattern: IUIAutomationTextPattern = match pattern {
    Some(p) => p,
    None => {
      return fail_ctx(
        result,
        "TextPatternNotSupported",
        "focused element does not expose TextPattern and no fallback was found".into(),
      )
    }
  };

  // --- selection ---------------------------------------------------------
  let selection: IUIAutomationTextRangeArray = match pattern.GetSelection() {
    Ok(s) => s,
    Err(e) => {
      return fail_ctx(
        result,
        "EmptySelection",
        format!("GetSelection() failed (usually: no active selection): {e}"),
      )
    }
  };

  let range_count = selection.Length().unwrap_or(0);
  if range_count <= 0 {
    return fail_ctx(
      result,
      "EmptySelection",
      "GetSelection() returned 0 ranges (caret only, no highlighted text)".into(),
    );
  }

  let mut texts: Vec<String> = Vec::new();
  let mut rects: Vec<SelectionRect> = Vec::new();
  for i in 0..range_count {
    let range: IUIAutomationTextRange = match selection.GetElement(i) {
      Ok(r) => r,
      Err(e) => {
        result.warnings.push(format!("range {i}: GetElement failed: {e}"));
        continue;
      }
    };
    match range.GetText(-1) {
      Ok(p) => {
        let mut s = p.to_string();
        if s.chars().count() > MAX_TEXT_CHARS {
          s = truncate(s, MAX_TEXT_CHARS);
          result.warnings.push(format!("text truncated to {MAX_TEXT_CHARS} chars"));
        }
        texts.push(s);
      }
      Err(e) => result.warnings.push(format!("range {i}: GetText failed: {e}")),
    }
    match range.GetBoundingRectangles() {
      Ok(sa_ptr) => match parse_rect_safearray(sa_ptr) {
        Ok(mut r) => rects.append(&mut r),
        Err(msg) => result
          .warnings
          .push(format!("range {i}: bounding rectangles unavailable: {msg}")),
      },
      Err(e) => result.warnings.push(format!("range {i}: GetBoundingRectangles failed: {e}")),
    }
  }

  let text = texts.join("\n");
  if text.trim().is_empty() {
    return fail_ctx(
      result,
      "EmptySelection",
      "selection ranges carry no text (caret-only / collapsed selection)".into(),
    );
  }

  result.success = true;
  result.text = Some(text);
  result.rect_count = rects.len() as u32;
  result.rects = rects;
  result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
  result
}

// ---------------------------------------------------------------------------
// UIA helpers
// ---------------------------------------------------------------------------

unsafe fn text_pattern(el: &IUIAutomationElement) -> Option<IUIAutomationTextPattern> {
  let p: IUIAutomationTextPattern = el.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
  if p.as_raw().is_null() {
    None
  } else {
    Some(p)
  }
}

unsafe fn el_name(el: &IUIAutomationElement) -> String {
  el.CurrentName().map(|b| b.to_string()).unwrap_or_default()
}

unsafe fn el_pid(el: &IUIAutomationElement) -> u32 {
  el.CurrentProcessId().map(|p| p as u32).unwrap_or(0)
}

unsafe fn control_type_name(el: &IUIAutomationElement) -> String {
  let ct = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
  let name = match ct {
    x if x == UIA_DocumentControlTypeId.0 => "Document",
    x if x == UIA_EditControlTypeId.0 => "Edit",
    50020 => "Text",
    50032 => "Window",
    50033 => "Pane",
    _ => "Other",
  };
  format!("{name} ({ct})")
}

/// `GetBoundingRectangles` returns a 1-D SAFEARRAY of f64 quadruples
/// (left, top, width, height) in physical screen pixels. UIA allocates it
/// with the OLE task allocator; release with SafeArrayDestroy.
unsafe fn parse_rect_safearray(sa_ptr: *mut SAFEARRAY) -> std::result::Result<Vec<SelectionRect>, String> {
  if sa_ptr.is_null() {
    return Err("null array".into());
  }
  let sa = &*sa_ptr;
  if sa.cDims != 1 {
    SafeArrayDestroy(sa_ptr).ok();
    return Err(format!("unexpected dimension count {}", sa.cDims));
  }
  if sa.cbElements != std::mem::size_of::<f64>() as u32 {
    SafeArrayDestroy(sa_ptr).ok();
    return Err(format!("unexpected element size {}", sa.cbElements));
  }
  let c_elements = sa.rgsabound[0].cElements as usize;
  if c_elements % 4 != 0 {
    SafeArrayDestroy(sa_ptr).ok();
    return Err(format!("element count {c_elements} not a multiple of 4"));
  }
  if c_elements / 4 > MAX_RECTS {
    SafeArrayDestroy(sa_ptr).ok();
    return Err(format!("rect count {} exceeds sanity limit {MAX_RECTS}", c_elements / 4));
  }

  let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
  if SafeArrayAccessData(sa_ptr, &mut data).is_err() || data.is_null() {
    SafeArrayDestroy(sa_ptr).ok();
    return Err("SafeArrayAccessData failed".into());
  }
  let rects: Vec<SelectionRect> = std::slice::from_raw_parts(data.cast::<f64>(), c_elements)
    .chunks_exact(4)
    .map(|v| SelectionRect { x: v[0], y: v[1], width: v[2], height: v[3] })
    .collect();
  SafeArrayUnaccessData(sa_ptr).ok();
  SafeArrayDestroy(sa_ptr).ok();
  Ok(rects)
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

unsafe fn window_text(hwnd: HWND) -> String {
  let mut buf = [0u16; 512];
  let len = GetWindowTextW(hwnd, &mut buf);
  if len <= 0 {
    return String::new();
  }
  String::from_utf16_lossy(&buf[..len as usize])
}

unsafe fn window_pid(hwnd: HWND) -> u32 {
  let mut pid: u32 = 0;
  GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
  pid
}

unsafe fn process_image_name(pid: u32) -> String {
  if pid == 0 {
    return "unknown".into();
  }
  let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
    return "unknown".into();
  };
  let mut buf = [0u16; 1024];
  let mut len = buf.len() as u32;
  let ok = QueryFullProcessImageNameW(
    handle,
    PROCESS_NAME_WIN32,
    PWSTR::from_raw(buf.as_mut_ptr()),
    &mut len,
  )
  .is_ok();
  let _ = CloseHandle(handle);
  if !ok || len == 0 {
    return "unknown".into();
  }
  let full = String::from_utf16_lossy(&buf[..len as usize]);
  full
    .rsplit(['\\', '/'])
    .next()
    .unwrap_or("unknown")
    .to_string()
}

fn truncate(s: String, max_chars: usize) -> String {
  if s.chars().count() <= max_chars {
    s
  } else {
    let t: String = s.chars().take(max_chars).collect();
    format!("{t}…")
  }
}

// ---------------------------------------------------------------------------
// napi Task entries: compute() runs on a libuv worker thread, resolves a
// Promise. Two thin wrappers share one worker/timeout mechanism —
// `probeSelection` for product code (slim), `probeSelectionDetailed` for the
// PoC audit log.
// ---------------------------------------------------------------------------

pub struct ProbeTask {
  pub timeout_ms: u32,
}

impl Task for ProbeTask {
  type Output = ProbeResult;
  type JsValue = SelectionResult;

  fn compute(&mut self) -> Result<Self::Output> {
    Ok(probe_with_timeout(Duration::from_millis(self.timeout_ms as u64)))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output.into())
  }
}

pub struct ProbeTaskDetailed {
  pub timeout_ms: u32,
}

impl Task for ProbeTaskDetailed {
  type Output = ProbeResult;
  type JsValue = ProbeResult;

  fn compute(&mut self) -> Result<Self::Output> {
    Ok(probe_with_timeout(Duration::from_millis(self.timeout_ms as u64)))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Probe the foreground window for a UIA text selection (product interface).
///
/// Never touches the clipboard. Resolves with a 4-field `SelectionResult`;
/// `errorCode` is `null` on success, otherwise `Timeout` / `EmptySelection` /
/// `TextPatternNotSupported` / … with empty `text` and `rects`.
#[napi(ts_return_type = "Promise<SelectionResult>")]
pub fn probe_selection(timeout_ms: Option<u32>) -> AsyncTask<ProbeTask> {
  AsyncTask::new(ProbeTask { timeout_ms: timeout_ms.unwrap_or(500) })
}

/// Full audit variant of `probeSelection` — every diagnostic field (focused
/// element, timing breakdown, warnings). Used by the PoC log
/// path; product code should prefer `probeSelection`.
#[napi(ts_return_type = "Promise<ProbeResult>")]
pub fn probe_selection_detailed(timeout_ms: Option<u32>) -> AsyncTask<ProbeTaskDetailed> {
  AsyncTask::new(ProbeTaskDetailed { timeout_ms: timeout_ms.unwrap_or(500) })
}
