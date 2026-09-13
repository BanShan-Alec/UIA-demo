import { app, globalShortcut } from 'electron'
import { appendFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { probeSelectionDetailed } from 'uia-probe'
import type { ProbeResult } from 'uia-probe'

// Product code only needs the 4-field interface:
//   import { probeSelection } from 'uia-probe'
//   const { text, rects, errorCode, processName } = await probeSelection(500)
//   // errorCode === null  -> translate `text`, anchor popup at rects[0]
//   // errorCode non-null  -> branch on EmptySelection / TextPatternNotSupported / Timeout / ...
// This entrypoint keeps the full audit structure for the PoC log (SPEC §4.1).

const HOTKEY = process.env.UIA_HOTKEY ?? 'Control+Shift+2'
const TIMEOUT_MS = Number(process.env.UIA_TIMEOUT_MS ?? 500)
const LOG_FILE = resolve(app.getAppPath(), 'uia-probe.log')

let seq = 0
let inFlight = false

type Trigger = 'hotkey' | 'selftest'

function emit(level: 'info' | 'probe' | 'error', payload: Record<string, unknown>): void {
  const line = JSON.stringify({ timestamp: new Date().toISOString(), level, ...payload })
  console.log(line)
  try {
    appendFileSync(LOG_FILE, line + '\n')
  } catch {
    /* log file is best-effort; console output remains the source of truth */
  }
}

async function handleProbe(trigger: Trigger): Promise<void> {
  const id = ++seq
  const t0 = performance.now()

  if (inFlight) {
    emit('probe', {
      seq: id,
      trigger,
      success: false,
      targetApp: null,
      text: null,
      rectCount: 0,
      rects: [],
      elapsedMs: Math.round(performance.now() - t0),
      errorCode: 'Busy',
      errorMessage: 'previous probe still in flight — request skipped'
    })
    return
  }
  inFlight = true

  let result: ProbeResult
  try {
    result = await probeSelectionDetailed(TIMEOUT_MS)
  } catch (err) {
    result = {
      success: false,
      processName: 'unknown',
      processId: 0,
      windowTitle: '',
      focusedElement: null,
      text: null,
      rectCount: 0,
      rects: [],
      nativeElapsedMs: 0,
      errorCode: 'NativeException',
      errorMessage: String(err),
      warnings: []
    }
  } finally {
    inFlight = false
  }

  // elapsedMs = end-to-end (hotkey press -> data returned), per spec;
  // nativeElapsedMs = time spent inside the Rust probe only.
  emit('probe', {
    seq: id,
    trigger,
    success: result.success,
    targetApp: `${result.processName} (pid ${result.processId}) — ${result.windowTitle || '<no title>'}`,
    processName: result.processName,
    processId: result.processId,
    windowTitle: result.windowTitle,
    focusedElement: result.focusedElement,
    text: result.text,
    rectCount: result.rectCount,
    rects: result.rects,
    elapsedMs: Math.round(performance.now() - t0),
    nativeElapsedMs: Math.round(result.nativeElapsedMs * 100) / 100,
    errorCode: result.errorCode ?? null,
    errorMessage: result.errorMessage ?? null,
    warnings: result.warnings.length > 0 ? result.warnings : undefined
  })
}

app.disableHardwareAcceleration()

app.whenReady().then(() => {
  const registered = globalShortcut.register(HOTKEY, () => {
    void handleProbe('hotkey')
  })

  emit('info', {
    msg: 'UIA selection probe ready',
    hotkey: HOTKEY,
    hotkeyRegistered: registered,
    timeoutMs: TIMEOUT_MS,
    logFile: LOG_FILE,
    electron: process.versions.electron,
    node: process.versions.node
  })

  if (!registered) {
    emit('error', { msg: `failed to register hotkey ${HOTKEY} (conflict with another app?)` })
  }

  // Optional automated testing path, independent of the hotkey:
  //   UIA_SELFTEST=1 UIA_SELFTEST_REPEAT=3 pnpm dev
  if (process.env.UIA_SELFTEST === '1') {
    const repeat = Math.max(1, Number(process.env.UIA_SELFTEST_REPEAT ?? 1))
    const interval = Math.max(300, Number(process.env.UIA_SELFTEST_INTERVAL_MS ?? 1500))
    let done = 0
    const run = (): void => {
      void handleProbe('selftest').then(() => {
        done += 1
        if (done < repeat) setTimeout(run, interval)
      })
    }
    setTimeout(run, 800)
  }
})

app.on('will-quit', () => globalShortcut.unregisterAll())
app.on('window-all-closed', () => {
  // PoC has no window to close; keep the process alive for hotkey probing.
})
