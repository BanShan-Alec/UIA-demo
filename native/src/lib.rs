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
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
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
  IUIAutomationTextRangeArray, CUIAutomation, TextPatternRangeEndpoint_End,
  TextPatternRangeEndpoint_Start, TextUnit, TextUnit_Character, TextUnit_Document,
  TextUnit_Format, TextUnit_Line, TextUnit_Page, TextUnit_Paragraph, TextUnit_Word,
  TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_DocumentControlTypeId,
  UIA_EditControlTypeId, UIA_IsTextPatternAvailablePropertyId, UIA_TextPatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
  EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

mod dot;

const MAX_TEXT_CHARS: usize = 10_000;
const MAX_RECTS: usize = 4_096;
/// Expanded unit text is a diagnostic payload, not a product return value.
/// Cap it so a Document/Page unit cannot stuff megabytes through napi.
const MAX_EXPAND_TEXT_CHARS: usize = 5_000;

// ---------------------------------------------------------------------------
// napi types
// ---------------------------------------------------------------------------

#[napi(object)]
#[derive(Clone)]
pub struct SelectionRect {
  /// Absolute screen coordinate (physical pixels) of the line fragment.
  pub x: f64,
  pub y: f64,
  pub width: f64,
  pub height: f64,
}

#[napi(object)]
#[derive(Clone)]
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

/// One `ExpandToEnclosingUnit` measurement. The interesting field is
/// `text_chars`: comparing it with `document_chars` reveals whether Chromium
/// honoured the requested unit or silently degraded to a larger one.
#[napi(object)]
pub struct UnitProbeResult {
  /// UIA TextUnit name: Character / Format / Word / Line / Paragraph / Page / Document.
  pub unit: String,
  pub unit_value: i32,
  /// "selection" = the live GetSelection range as-is;
  /// "collapsed-start" = the same range collapsed to its start endpoint.
  pub mode: String,
  /// Character count of the expanded range before transport truncation.
  pub text_chars: u32,
  /// First ~160 characters with line breaks escaped, for logs.
  pub text_preview: String,
  /// Expanded range text (truncated to MAX_EXPAND_TEXT_CHARS).
  pub text: String,
  pub text_truncated: bool,
  pub rect_count: u32,
  /// True when the expanded range covers the whole document — the degradation
  /// signal described in the Snapword PRD (Paragraph → Page/Document).
  pub equals_document: bool,
  pub warnings: Vec<String>,
}

/// Result of the ExpandToEnclosingUnit probe (audit-only API).
#[napi(object)]
pub struct ExpandProbeResult {
  pub success: bool,
  pub process_name: String,
  pub process_id: u32,
  pub window_title: String,
  pub focused_element: Option<FocusInfo>,
  /// How many ranges GetSelection() returned (usually 1 in Chromium).
  pub selection_ranges: u32,
  pub selection_chars: u32,
  pub selection_text: String,
  /// DocumentRange text length — the baseline for the degradation check.
  pub document_chars: u32,
  pub document_preview: String,
  pub results: Vec<UnitProbeResult>,
  pub native_elapsed_ms: f64,
  pub error_code: Option<String>,
  pub error_message: Option<String>,
  pub warnings: Vec<String>,
}

impl ExpandProbeResult {
  fn err(code: &str, msg: impl Into<String>, start: Instant) -> Self {
    ExpandProbeResult {
      success: false,
      process_name: String::new(),
      process_id: 0,
      window_title: String::new(),
      focused_element: None,
      selection_ranges: 0,
      selection_chars: 0,
      selection_text: String::new(),
      document_chars: 0,
      document_preview: String::new(),
      results: Vec::new(),
      native_elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
      error_code: Some(code.to_string()),
      error_message: Some(msg.into()),
      warnings: Vec::new(),
    }
  }

  fn from_probe_error(err: ProbeResult, start: Instant) -> Self {
    ExpandProbeResult {
      success: false,
      process_name: err.process_name,
      process_id: err.process_id,
      window_title: err.window_title,
      focused_element: err.focused_element,
      selection_ranges: 0,
      selection_chars: 0,
      selection_text: String::new(),
      document_chars: 0,
      document_preview: String::new(),
      results: Vec::new(),
      native_elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
      error_code: err.error_code,
      error_message: err.error_message,
      warnings: err.warnings,
    }
  }
}

// ---------------------------------------------------------------------------
// dedicated UIA worker thread (COM MTA)
// ---------------------------------------------------------------------------

enum ProbeCmd {
  Probe { reply: Sender<ProbeResult> },
  ProbeWindow {
    hwnd: isize,
    reply: Sender<ProbeResult>,
  },
  Expand {
    reply: Sender<ExpandProbeResult>,
    window_title: Option<String>,
  },
  ExpandWindow {
    hwnd: isize,
    reply: Sender<ExpandProbeResult>,
  },
}

enum WorkerError {
  StartFailed,
  Timeout,
  Disconnected,
}

struct WorkerHandle {
  tx: Sender<ProbeCmd>,
}

static WORKER: OnceLock<Mutex<Option<WorkerHandle>>> = OnceLock::new();
static WORKER_SEQ: AtomicU64 = AtomicU64::new(0);

fn worker_slot() -> &'static Mutex<Option<WorkerHandle>> {
  WORKER.get_or_init(|| Mutex::new(None))
}

/// Run one command on the shared COM MTA worker with a hard timeout. Called
/// from a `napi::Task::compute` (libuv thread); the actual COM work happens on
/// the worker thread.
fn run_on_worker<T: Send + 'static>(
  timeout: Duration,
  make_cmd: impl Fn(Sender<T>) -> ProbeCmd,
) -> std::result::Result<T, WorkerError> {
  let (reply_tx, reply_rx) = channel::<T>();

  {
    let mut guard = worker_slot().lock().unwrap_or_else(|e| e.into_inner());
    // The worker may have died (stuck probe abandoned earlier); retry once
    // with a freshly spawned worker.
    let mut sent = guard
      .as_ref()
      .map(|w| w.tx.send(make_cmd(reply_tx.clone())).is_ok())
      .unwrap_or(false);
    if !sent {
      *guard = spawn_worker();
      sent = guard
        .as_ref()
        .map(|w| w.tx.send(make_cmd(reply_tx.clone())).is_ok())
        .unwrap_or(false);
    }
    if !sent {
      return Err(WorkerError::StartFailed);
    }
  }

  match reply_rx.recv_timeout(timeout) {
    Ok(result) => Ok(result),
    Err(RecvTimeoutError::Timeout) => {
      // Abandon the stuck worker; the next command spawns a fresh one. The old
      // thread exits on its own once its channel is disconnected.
      let mut guard = worker_slot().lock().unwrap_or_else(|e| e.into_inner());
      *guard = None;
      Err(WorkerError::Timeout)
    }
    Err(RecvTimeoutError::Disconnected) => Err(WorkerError::Disconnected),
  }
}

fn probe_with_timeout(timeout: Duration) -> ProbeResult {
  let start = Instant::now();
  match run_on_worker(timeout, |reply| ProbeCmd::Probe { reply }) {
    Ok(result) => result,
    Err(WorkerError::StartFailed) => ProbeResult::err(
      "ComInitFailed",
      "failed to start UIA worker thread",
      start,
    ),
    Err(WorkerError::Timeout) => ProbeResult::err(
      "Timeout",
      format!(
        "UIA probe did not complete within {} ms (target app may be unresponsive)",
        timeout.as_millis()
      ),
      start,
    ),
    Err(WorkerError::Disconnected) => ProbeResult::err(
      "ComError",
      "UIA worker thread exited unexpectedly",
      start,
    ),
  }
}

fn expand_with_timeout(timeout: Duration, window_title: Option<String>) -> ExpandProbeResult {
  let start = Instant::now();
  match run_on_worker(timeout, |reply| ProbeCmd::Expand {
    reply,
    window_title: window_title.clone(),
  }) {
    Ok(result) => result,
    Err(WorkerError::StartFailed) => ExpandProbeResult::err(
      "ComInitFailed",
      "failed to start UIA worker thread",
      start,
    ),
    Err(WorkerError::Timeout) => ExpandProbeResult::err(
      "Timeout",
      format!(
        "UIA expand probe did not complete within {} ms (target app may be unresponsive)",
        timeout.as_millis()
      ),
      start,
    ),
    Err(WorkerError::Disconnected) => ExpandProbeResult::err(
      "ComError",
      "UIA worker thread exited unexpectedly",
      start,
    ),
  }
}

/// Dot-trigger path: read the live selection from a specific target window.
/// Used by the native dot thread at mouse-up and at click time.
pub(crate) fn probe_selection_for_window_sync(
  hwnd: isize,
  timeout: Duration,
) -> SelectionResult {
  let start = Instant::now();
  let result = match run_on_worker(timeout, |reply| ProbeCmd::ProbeWindow { hwnd, reply }) {
    Ok(result) => result,
    Err(WorkerError::StartFailed) => ProbeResult::err(
      "ComInitFailed",
      "failed to start UIA worker thread",
      start,
    ),
    Err(WorkerError::Timeout) => ProbeResult::err(
      "Timeout",
      format!(
        "dot selection probe did not complete within {} ms",
        timeout.as_millis()
      ),
      start,
    ),
    Err(WorkerError::Disconnected) => ProbeResult::err(
      "ComError",
      "UIA worker thread exited unexpectedly",
      start,
    ),
  };
  result.into()
}

/// Dot-trigger path: read the enclosing paragraph from a specific target
/// window. The dot thread calls this after `probe_selection_for_window_sync`.
pub(crate) fn probe_expand_for_window_sync(hwnd: isize, timeout: Duration) -> ExpandProbeResult {
  let start = Instant::now();
  match run_on_worker(timeout, |reply| ProbeCmd::ExpandWindow { hwnd, reply }) {
    Ok(result) => result,
    Err(WorkerError::StartFailed) => ExpandProbeResult::err(
      "ComInitFailed",
      "failed to start UIA worker thread",
      start,
    ),
    Err(WorkerError::Timeout) => ExpandProbeResult::err(
      "Timeout",
      format!(
        "dot paragraph probe did not complete within {} ms",
        timeout.as_millis()
      ),
      start,
    ),
    Err(WorkerError::Disconnected) => ExpandProbeResult::err(
      "ComError",
      "UIA worker thread exited unexpectedly",
      start,
    ),
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
        match cmd {
          ProbeCmd::Probe { reply } => {
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
          ProbeCmd::ProbeWindow { hwnd, reply } => {
            let result = if com_init.is_ok() {
              unsafe { run_probe_for_hwnd(Some(HWND(hwnd as *mut core::ffi::c_void))) }
            } else {
              ProbeResult::err(
                "ComInitFailed",
                format!("CoInitializeEx failed: {com_init:?}"),
                Instant::now(),
              )
            };
            let _ = reply.send(result);
          }
          ProbeCmd::Expand { reply, window_title } => {
            let result = if com_init.is_ok() {
              let target = match window_title {
                Some(title) => ExpandTarget::Title(title),
                None => ExpandTarget::Foreground,
              };
              unsafe { run_expand_probe(target) }
            } else {
              ExpandProbeResult::err(
                "ComInitFailed",
                format!("CoInitializeEx failed: {com_init:?}"),
                Instant::now(),
              )
            };
            let _ = reply.send(result);
          }
          ProbeCmd::ExpandWindow { hwnd, reply } => {
            let result = if com_init.is_ok() {
              unsafe { run_expand_probe(ExpandTarget::Hwnd(hwnd)) }
            } else {
              ExpandProbeResult::err(
                "ComInitFailed",
                format!("CoInitializeEx failed: {com_init:?}"),
                Instant::now(),
              )
            };
            let _ = reply.send(result);
          }
        }
      }
      unsafe {
        windows::Win32::System::Com::CoUninitialize();
      }
    });
  spawned.ok().map(|_| WorkerHandle { tx })
}

// ---------------------------------------------------------------------------
// the actual probe
// ---------------------------------------------------------------------------

/// Foreground window plus the TextPattern that reflects its live selection.
struct Target {
  process_name: String,
  process_id: u32,
  window_title: String,
  focused_element: Option<FocusInfo>,
  pattern: IUIAutomationTextPattern,
}

/// Locate the foreground app's TextPattern. Shared by `run_probe` and
/// `run_expand_probe` so both use exactly the same focused-element /
/// tree-search strategy and failure codes.
unsafe fn locate_target(start: Instant) -> std::result::Result<Target, ProbeResult> {
  // --- foreground window -------------------------------------------------
  let hwnd: HWND = GetForegroundWindow();
  if hwnd.0.is_null() {
    return Err(ProbeResult::err(
      "NoForegroundWindow",
      "GetForegroundWindow returned NULL",
      start,
    ));
  }
  locate_target_for_hwnd(hwnd, start)
}

/// Locate a TextPattern for a specific top-level window. The dot thread
/// captures the target HWND at mouse-up and must keep reading that window even
/// if clicking the dot changes system focus.
unsafe fn locate_target_for_hwnd(
  hwnd: HWND,
  start: Instant,
) -> std::result::Result<Target, ProbeResult> {
  if hwnd.0.is_null() {
    return Err(ProbeResult::err(
      "NoForegroundWindow",
      "target HWND is null",
      start,
    ));
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

  // Same as the fail path in `run_probe`, but carries the foreground-window
  // context that was already collected. A closure (not a macro) so the
  // captured variables resolve normally.
  let fail_ctx = |mut result: ProbeResult, code: &str, msg: String| -> ProbeResult {
    result.success = false;
    result.error_code = Some(code.to_string());
    result.error_message = Some(msg);
    result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result
  };

  // --- UIA client --------------------------------------------------------
  let automation: IUIAutomation =
    match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
      Ok(a) => a,
      Err(e) => {
        return Err(fail_ctx(
          result,
          "ComError",
          format!("CoCreateInstance(CLSID_CUIAutomation) failed: {e}"),
        ))
      }
    };

  // Primary strategy: the focused element. In Chromium the document (or the
  // focused text field) carries the TextPattern and reflects the live
  // selection. This is the fast path for hot calls.
  let focused: IUIAutomationElement = match automation.GetFocusedElement() {
    Ok(el) => el,
    Err(e) => {
      return Err(fail_ctx(
        result,
        "ElementNotFound",
        format!("GetFocusedElement failed: {e}"),
      ))
    }
  };

  if focused
    .CurrentIsPassword()
    .map(|b| b.as_bool())
    .unwrap_or(false)
  {
    return Err(fail_ctx(
      result,
      "PasswordField",
      "focused element is a password field".into(),
    ));
  }

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
    let condition =
      match automation.CreatePropertyCondition(UIA_IsTextPatternAvailablePropertyId, &flag) {
        Ok(c) => c,
        Err(e) => {
          return Err(fail_ctx(
            result,
            "ComError",
            format!("CreatePropertyCondition failed: {e}"),
          ))
        }
      };
    let root: IUIAutomationElement = match automation.ElementFromHandle(hwnd) {
      Ok(r) => r,
      Err(e) => {
        return Err(fail_ctx(
          result,
          "ElementNotFound",
          format!("ElementFromHandle(foreground) failed: {e}"),
        ))
      }
    };
    match root.FindFirst(TreeScope_Descendants, &condition) {
      Ok(el) => {
        if el.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false) {
          result.focused_element = Some(focus_info);
          return Err(fail_ctx(
            result,
            "PasswordField",
            "fallback element is a password field".into(),
          ));
        }
        pattern = text_pattern(&el);
      }
      Err(_) => {
        result.focused_element = Some(focus_info);
        return Err(fail_ctx(
          result,
          "TextPatternNotSupported",
          "no element exposing TextPattern found in foreground window (compatibility boundary, e.g. Chromium PDF viewer)".into(),
        ));
      }
    }
  }
  result.focused_element = Some(focus_info);

  let pattern: IUIAutomationTextPattern = match pattern {
    Some(p) => p,
    None => {
      return Err(fail_ctx(
        result,
        "TextPatternNotSupported",
        "focused element does not expose TextPattern and no fallback was found".into(),
      ))
    }
  };

  Ok(Target {
    process_name,
    process_id: pid,
    window_title,
    focused_element: result.focused_element,
    pattern,
  })
}

struct WindowSearch {
  needle: String,
  found: Option<HWND>,
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
  if !IsWindowVisible(hwnd).as_bool() {
    return BOOL(1);
  }
  // SAFETY: EnumWindows is synchronous; the pointer stays valid for the call.
  let search = &mut *(lparam.0 as *mut WindowSearch);
  let title = window_text(hwnd);
  if title.to_lowercase().contains(&search.needle) {
    search.found = Some(hwnd);
    return BOOL(0); // stop
  }
  BOOL(1)
}

/// Find a visible top-level window by title substring (case-insensitive).
/// Used by the automated ExpandToEnclosingUnit probe, where the calling
/// process may have no foreground window of its own.
unsafe fn find_window_by_title(needle: &str) -> Option<HWND> {
  let mut search = WindowSearch {
    needle: needle.to_lowercase(),
    found: None,
  };
  let _ = EnumWindows(
    Some(enum_window_proc),
    LPARAM(&mut search as *mut WindowSearch as isize),
  );
  search.found
}

/// Locate a TextPattern by window title instead of by foreground focus.
/// Prefers ControlType=Document (the web content) and falls back to the first
/// element exposing TextPattern.
unsafe fn locate_target_by_title(
  needle: &str,
  start: Instant,
) -> std::result::Result<Target, ProbeResult> {
  let hwnd = match find_window_by_title(needle) {
    Some(h) => h,
    None => {
      return Err(ProbeResult::err(
        "WindowNotFound",
        format!("no visible window title contains {needle:?}"),
        start,
      ))
    }
  };

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

  let fail_ctx = |mut result: ProbeResult, code: &str, msg: String| -> ProbeResult {
    result.success = false;
    result.error_code = Some(code.to_string());
    result.error_message = Some(msg);
    result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result
  };

  let automation: IUIAutomation =
    match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
      Ok(a) => a,
      Err(e) => {
        return Err(fail_ctx(
          result,
          "ComError",
          format!("CoCreateInstance(CLSID_CUIAutomation) failed: {e}"),
        ))
      }
    };

  let root: IUIAutomationElement = match automation.ElementFromHandle(hwnd) {
    Ok(r) => r,
    Err(e) => {
      return Err(fail_ctx(
        result,
        "ElementNotFound",
        format!("ElementFromHandle(title match) failed: {e}"),
      ))
    }
  };

  let mut focus_info = FocusInfo {
    name: truncate(window_title.clone(), 120),
    control_type: "Window".into(),
    via: "window-title".into(),
    process_id: pid,
  };

  let mut pattern = None;

  // First choice: the web document, which owns the live selection.
  if let Ok(doc_cond) = automation.CreatePropertyCondition(
    UIA_ControlTypePropertyId,
    &VARIANT::from(UIA_DocumentControlTypeId.0),
  ) {
    if let Ok(doc) = root.FindFirst(TreeScope_Descendants, &doc_cond) {
      if !doc.as_raw().is_null() {
        focus_info.name = truncate(el_name(&doc), 120);
        focus_info.control_type = control_type_name(&doc);
        focus_info.process_id = el_pid(&doc);
        pattern = text_pattern(&doc);
      }
    }
  }

  // Fallback: any element exposing TextPattern.
  if pattern.is_none() {
    let flag = VARIANT::from(true);
    let condition =
      match automation.CreatePropertyCondition(UIA_IsTextPatternAvailablePropertyId, &flag) {
        Ok(c) => c,
        Err(e) => {
          return Err(fail_ctx(
            result,
            "ComError",
            format!("CreatePropertyCondition failed: {e}"),
          ))
        }
      };
    match root.FindFirst(TreeScope_Descendants, &condition) {
      Ok(el) => {
        focus_info.name = truncate(el_name(&el), 120);
        focus_info.control_type = control_type_name(&el);
        focus_info.process_id = el_pid(&el);
        pattern = text_pattern(&el);
      }
      Err(_) => {
        result.focused_element = Some(focus_info);
        return Err(fail_ctx(
          result,
          "TextPatternNotSupported",
          "no element exposing TextPattern found in the title-matched window".into(),
        ));
      }
    }
  }

  result.focused_element = Some(focus_info);
  let pattern: IUIAutomationTextPattern = match pattern {
    Some(p) => p,
    None => {
      return Err(fail_ctx(
        result,
        "TextPatternNotSupported",
        "title-matched window has no TextPattern-capable element".into(),
      ))
    }
  };

  Ok(Target {
    process_name,
    process_id: pid,
    window_title,
    focused_element: result.focused_element,
    pattern,
  })
}

unsafe fn run_probe() -> ProbeResult {
  run_probe_for_hwnd(None)
}

unsafe fn run_probe_for_hwnd(hwnd: Option<HWND>) -> ProbeResult {
  let start = Instant::now();
  let target = match hwnd {
    Some(h) => locate_target_for_hwnd(h, start),
    None => locate_target(start),
  };
  let target = match target {
    Ok(t) => t,
    Err(e) => return e,
  };

  let mut result = ProbeResult {
    success: false,
    process_name: target.process_name,
    process_id: target.process_id,
    window_title: target.window_title,
    focused_element: target.focused_element,
    text: None,
    rect_count: 0,
    rects: Vec::new(),
    native_elapsed_ms: 0.0,
    error_code: None,
    error_message: None,
    warnings: Vec::new(),
  };

  let fail_ctx = |mut result: ProbeResult, code: &str, msg: String| -> ProbeResult {
    result.success = false;
    result.error_code = Some(code.to_string());
    result.error_message = Some(msg);
    result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result
  };

  let pattern = target.pattern;

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
// ExpandToEnclosingUnit probe (audit-only)
// ---------------------------------------------------------------------------

/// Expand the live selection with every UIA TextUnit, in two modes:
///
/// * `selection`       — the GetSelection() range exactly as selected
/// * `collapsed-start` — the same range collapsed to its start endpoint
///                        (the workflow for "paragraph containing the start
///                        of the selection")
///
/// `ExpandToEnclosingUnit` normalizes a range to the smallest enclosing unit;
/// it never shrinks a range that is already larger than the unit. Comparing
/// each result's character count with the document length exposes silent
/// degradation to Page/Document (the Snapword PRD's Q-1 risk).
enum ExpandTarget {
  Foreground,
  Title(String),
  Hwnd(isize),
}

unsafe fn run_expand_probe(target: ExpandTarget) -> ExpandProbeResult {
  let start = Instant::now();
  let target = match target {
    ExpandTarget::Foreground => locate_target(start),
    ExpandTarget::Title(title) => locate_target_by_title(&title, start),
    ExpandTarget::Hwnd(hwnd) => {
      locate_target_for_hwnd(HWND(hwnd as *mut core::ffi::c_void), start)
    }
  };
  let target = match target {
    Ok(t) => t,
    Err(e) => return ExpandProbeResult::from_probe_error(e, start),
  };

  let mut result = ExpandProbeResult {
    success: false,
    process_name: target.process_name,
    process_id: target.process_id,
    window_title: target.window_title,
    focused_element: target.focused_element,
    selection_ranges: 0,
    selection_chars: 0,
    selection_text: String::new(),
    document_chars: 0,
    document_preview: String::new(),
    results: Vec::new(),
    native_elapsed_ms: 0.0,
    error_code: None,
    error_message: None,
    warnings: Vec::new(),
  };

  let fail_ctx = |mut result: ExpandProbeResult, code: &str, msg: String| -> ExpandProbeResult {
    result.success = false;
    result.error_code = Some(code.to_string());
    result.error_message = Some(msg);
    result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    result
  };

  let pattern = target.pattern;

  // Baseline: the whole document the TextPattern exposes.
  let document_range = match pattern.DocumentRange() {
    Ok(r) => r,
    Err(e) => {
      return fail_ctx(
        result,
        "NoDocumentRange",
        format!("DocumentRange() failed: {e}"),
      )
    }
  };
  let document_text = match document_range.GetText(-1) {
    Ok(t) => t.to_string(),
    Err(e) => {
      return fail_ctx(
        result,
        "DocumentTextFailed",
        format!("DocumentRange.GetText failed: {e}"),
      )
    }
  };
  result.document_chars = document_text.chars().count() as u32;
  result.document_preview = preview(&document_text, 160);

  // The live selection is the same range the product probe reads.
  let selection = match pattern.GetSelection() {
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
  result.selection_ranges = range_count as u32;
  if range_count <= 0 {
    return fail_ctx(
      result,
      "EmptySelection",
      "GetSelection() returned 0 ranges (caret only, no highlighted text)".into(),
    );
  }

  let selection_range: IUIAutomationTextRange = match selection.GetElement(0) {
    Ok(r) => r,
    Err(e) => {
      return fail_ctx(
        result,
        "EmptySelection",
        format!("GetElement(0) failed: {e}"),
      )
    }
  };
  let selection_text = match selection_range.GetText(-1) {
    Ok(t) => t.to_string(),
    Err(e) => {
      return fail_ctx(
        result,
        "EmptySelection",
        format!("selection GetText failed: {e}"),
      )
    }
  };
  result.selection_chars = selection_text.chars().count() as u32;
  result.selection_text = cap_expand_text(&selection_text);

  // Two starting points: as-selected, and collapsed to the start endpoint.
  let mut bases: Vec<(&str, IUIAutomationTextRange)> =
    vec![("selection", selection_range.clone())];
  match selection_range.Clone() {
    Ok(collapsed) => match collapsed.MoveEndpointByRange(
      TextPatternRangeEndpoint_End,
      &selection_range,
      TextPatternRangeEndpoint_Start,
    ) {
      Ok(()) => bases.push(("collapsed-start", collapsed)),
      Err(e) => result
        .warnings
        .push(format!("collapsed-start mode unavailable: {e}")),
    },
    Err(e) => result
      .warnings
      .push(format!("collapsed-start clone failed: {e}")),
  }

  const UNITS: [(&str, TextUnit); 7] = [
    ("Character", TextUnit_Character),
    ("Format", TextUnit_Format),
    ("Word", TextUnit_Word),
    ("Line", TextUnit_Line),
    ("Paragraph", TextUnit_Paragraph),
    ("Page", TextUnit_Page),
    ("Document", TextUnit_Document),
  ];

  for (mode, base) in &bases {
    for (unit_name, unit) in UNITS {
      match expand_unit(base, unit, &document_text) {
        Ok(mut item) => {
          item.mode = (*mode).to_string();
          item.unit = unit_name.to_string();
          result.results.push(item);
        }
        Err(e) => result
          .warnings
          .push(format!("{mode}/{unit_name}: {e}")),
      }
    }
  }

  result.success = !result.results.is_empty();
  result.native_elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
  result
}

/// Clone the base range, expand it to `unit`, and measure the result. The
/// clone is essential: ExpandToEnclosingUnit mutates the receiver.
unsafe fn expand_unit(
  base: &IUIAutomationTextRange,
  unit: TextUnit,
  document_text: &str,
) -> std::result::Result<UnitProbeResult, String> {
  let range = base.Clone().map_err(|e| format!("Clone failed: {e}"))?;
  range
    .ExpandToEnclosingUnit(unit)
    .map_err(|e| format!("ExpandToEnclosingUnit failed: {e}"))?;
  let text = range
    .GetText(-1)
    .map_err(|e| format!("GetText failed: {e}"))?
    .to_string();
  let text_char_count = text.chars().count();

  let mut warnings = Vec::new();
  let rect_count = match range.GetBoundingRectangles() {
    Ok(sa_ptr) => match parse_rect_safearray(sa_ptr) {
      Ok(rects) => rects.len() as u32,
      Err(msg) => {
        warnings.push(format!("bounding rectangles unavailable: {msg}"));
        0
      }
    },
    Err(e) => {
      warnings.push(format!("GetBoundingRectangles failed: {e}"));
      0
    }
  };

  Ok(UnitProbeResult {
    unit: String::new(),
    unit_value: unit.0,
    mode: String::new(),
    text_chars: text_char_count as u32,
    text_preview: preview(&text, 160),
    text: cap_expand_text(&text),
    text_truncated: text_char_count > MAX_EXPAND_TEXT_CHARS,
    rect_count,
    equals_document: text == document_text,
    warnings,
  })
}

fn cap_expand_text(s: &str) -> String {
  if s.chars().count() <= MAX_EXPAND_TEXT_CHARS {
    s.to_string()
  } else {
    s.chars().take(MAX_EXPAND_TEXT_CHARS).collect()
  }
}

/// Log-friendly one-line preview: control characters are escaped so a whole
/// paragraph stays on one line in the JSON log / console.
fn preview(s: &str, max_chars: usize) -> String {
  let mut out = String::new();
  for ch in s.chars().take(max_chars) {
    match ch {
      '\r' => out.push_str("\\r"),
      '\n' => out.push_str("\\n"),
      '\t' => out.push_str("\\t"),
      c => out.push(c),
    }
  }
  if s.chars().count() > max_chars {
    out.push('…');
  }
  out
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

pub struct ExpandTask {
  pub timeout_ms: u32,
  pub window_title: Option<String>,
}

impl Task for ExpandTask {
  type Output = ExpandProbeResult;
  type JsValue = ExpandProbeResult;

  fn compute(&mut self) -> Result<Self::Output> {
    Ok(expand_with_timeout(
      Duration::from_millis(self.timeout_ms as u64),
      self.window_title.clone(),
    ))
  }

  fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
    Ok(output)
  }
}

/// Measure `ExpandToEnclosingUnit` for every UIA TextUnit on the live
/// selection. Audit-only: not part of the product interface.
///
/// Use it to verify the Snapword Q-1 question: does requesting Paragraph
/// return exactly the enclosing paragraph, or does Chromium silently degrade
/// to Page/Document (which would make the whole-page-to-LLM risk real)?
///
/// `windowTitle` is optional and only for automation: when present the probe
/// finds a visible top-level window by title substring instead of reading
/// `GetForegroundWindow`. Useful in test runners that have no foreground
/// window of their own. Omit it for the normal hotkey path.
#[napi(ts_return_type = "Promise<ExpandProbeResult>")]
pub fn probe_expand_units(
  timeout_ms: Option<u32>,
  window_title: Option<String>,
) -> AsyncTask<ExpandTask> {
  AsyncTask::new(ExpandTask {
    timeout_ms: timeout_ms.unwrap_or(500),
    window_title,
  })
}
