//! Standalone investigation: does Chromium's document-level TextPattern return
//! bounding rectangles for a programmatically selected range? Selects a range
//! inside the "UIA-TEST" Edge/Chrome tab via IUIAutomationTextRange::Select()
//! (no GUI interaction, works on a background window), then dumps text + rects.

use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::System::Com::{
  CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayUnaccessData};
use windows::Win32::UI::Accessibility::{
  IUIAutomation, IUIAutomationTextPattern, IUIAutomationTextRange, CUIAutomation,
  UIA_ControlTypePropertyId, UIA_DocumentControlTypeId, UIA_IsTextPatternAvailablePropertyId,
  UIA_TextPatternId, TreeScope_Descendants,
};
use windows::Win32::UI::WindowsAndMessaging::{
  EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

use std::sync::atomic::{AtomicIsize, Ordering};
static TARGET: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
  if !IsWindowVisible(hwnd).as_bool() {
    return BOOL(1);
  }
  let mut buf = [0u16; 256];
  let len = GetWindowTextW(hwnd, &mut buf);
  let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
  if title.contains("UIA-TEST") {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    println!("candidate window: {title:?} hwnd={:p} pid={pid}", hwnd.0);
    TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
    return BOOL(0); // stop
  }
  BOOL(1)
}

fn main() -> windows::core::Result<()> {
  unsafe {
    CoInitializeEx(None, COINIT_MULTITHREADED);
    EnumWindows(Some(enum_proc), LPARAM(0));
    let hwnd = HWND(TARGET.load(Ordering::SeqCst) as *mut core::ffi::c_void);
    if hwnd.0.is_null() {
      println!("RESULT: no window titled *UIA-TEST* found");
      return Ok(());
    }

    let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
    let root = automation.ElementFromHandle(hwnd)?;

    // 1) List every element in the window subtree that exposes TextPattern,
    //    with its document-range text length — this reveals whether the web
    //    document (renderer a11y tree) is fully exposed or in "lite" mode.
    let flag = VARIANT::from(true);
    let cond_tp = automation.CreatePropertyCondition(UIA_IsTextPatternAvailablePropertyId, &flag)?;
    let all = root.FindAll(TreeScope_Descendants, &cond_tp)?;
    println!("elements exposing TextPattern: {}", all.Length().unwrap_or(0));
    for i in 0..all.Length().unwrap_or(0) {
      let el = all.GetElement(i)?;
      let ct = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
      let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
      let pattern: IUIAutomationTextPattern = el.GetCurrentPatternAs(UIA_TextPatternId)?;
      let doc_chars = pattern
        .DocumentRange()
        .and_then(|r| r.GetText(4000))
        .map(|b| b.to_string())
        .map(|s| s.chars().count())
        .unwrap_or(0);
      println!(
        "  tp el[{i}]: controltype={ct} name={:?} doc_range_chars={doc_chars}",
        name.chars().take(40).collect::<String>()
      );
    }

    // 2) Focus on the web document: first element with ControlType=Document.
    let doc_cond = automation.CreatePropertyCondition(
      UIA_ControlTypePropertyId,
      &VARIANT::from(UIA_DocumentControlTypeId.0),
    )?;
    let doc = root.FindFirst(TreeScope_Descendants, &doc_cond)?;
    if doc.as_raw().is_null() {
      println!("RESULT: no ControlType=Document element in window subtree (renderer a11y tree not exposed)");
    } else {
      println!(
        "document element: name={:?} pid={}",
        doc.CurrentName().map(|b| b.to_string()).unwrap_or_default(),
        doc.CurrentProcessId().unwrap_or(-1)
      );
      let pattern: IUIAutomationTextPattern = doc.GetCurrentPatternAs(UIA_TextPatternId)?;
      let dr = pattern.DocumentRange()?;
      let full_text = dr.GetText(-1)?.to_string();
      println!("DocumentRange full text chars={}", full_text.chars().count());
      match dr.GetBoundingRectangles() {
        Ok(sa) => { dump_rects(sa); }
        Err(e) => println!("DocumentRange rects ERROR {e}"),
      }

      // Programmatically select the first 120 chars, then read back the
      // live selection the same way the probe addon does.
      let slice = dr.Clone()?;
      slice.Select()?;
      let sel = pattern.GetSelection()?;
      println!("selection ranges after Select(): {}", sel.Length().unwrap_or(0));
      for i in 0..sel.Length().unwrap_or(0) {
        let r: IUIAutomationTextRange = sel.GetElement(i)?;
        let t = r.GetText(-1)?.to_string();
        println!("  sel[{i}] text_len={} preview={:?}", t.chars().count(), t.chars().take(50).collect::<String>());
        match r.GetBoundingRectangles() {
          Ok(sa) => { dump_rects(sa); }
          Err(e) => println!("  sel[{i}] rects ERROR {e}"),
        }
      }
    }
  }
  Ok(())
}

unsafe fn dump_rects(sa_ptr: *mut SAFEARRAY) -> usize {
  if sa_ptr.is_null() {
    println!("  rects: null SAFEARRAY");
    return 0;
  }
  let sa = &*sa_ptr;
  println!(
    "  rects: dims={} cb={} elems={} lb={}",
    sa.cDims, sa.cbElements, sa.rgsabound[0].cElements, sa.rgsabound[0].lLbound
  );
  if sa.cDims != 1 || sa.cbElements as usize != std::mem::size_of::<f64>() {
    SafeArrayDestroy(sa_ptr).ok();
    return 0;
  }
  let n = sa.rgsabound[0].cElements as usize / 4;
  let mut data: *mut core::ffi::c_void = core::ptr::null_mut();
  if n > 0 && SafeArrayAccessData(sa_ptr, &mut data).is_ok() {
    for (i, v) in std::slice::from_raw_parts(data.cast::<f64>(), n * 4).chunks_exact(4).enumerate().take(8) {
      println!("  rect[{i}] = x:{} y:{} w:{} h:{}", v[0], v[1], v[2], v[3]);
    }
    SafeArrayUnaccessData(sa_ptr).ok();
  }
  SafeArrayDestroy(sa_ptr).ok();
  n
}
