import { app, BrowserWindow, globalShortcut, ipcMain } from 'electron'
import { appendFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { probeSelectionDetailed } from 'uia-probe'
import type { ProbeResult } from 'uia-probe'
import type { AppConfig, ProbeRecord, SetConfigResult } from '../shared/types'
import { loadConfig, saveConfig } from './config'

// Product code only needs the 4-field interface:
//   import { probeSelection } from 'uia-probe'
//   const { text, rects, errorCode, processName } = await probeSelection(500)
//   // errorCode === null  -> translate `text`, anchor popup at rects[0]
//   // errorCode non-null  -> branch on EmptySelection / TextPatternNotSupported / Timeout / ...
// This entrypoint keeps the full audit structure for the PoC log (SPEC §4.1).

const LOG_FILE = resolve(app.getAppPath(), 'uia-probe.log')
const HISTORY_CAP = 20

let config: AppConfig = loadConfig()
let seq = 0
let inFlight = false
let history: ProbeRecord[] = []
let win: BrowserWindow | null = null
let quitting = false

type Trigger = 'hotkey' | 'selftest' | 'manual'

function emit(level: 'info' | 'probe' | 'error', payload: object): void {
  const line = JSON.stringify({ timestamp: new Date().toISOString(), level, ...payload })
  console.log(line)
  try {
    appendFileSync(LOG_FILE, line + '\n')
  } catch {
    /* log file is best-effort; console output remains the source of truth */
  }
}

function publishProbe(record: ProbeRecord): void {
  emit('probe', record)
  history = [record, ...history].slice(0, HISTORY_CAP)
  win?.webContents.send('probe:result', record)
}

async function handleProbe(trigger: Trigger): Promise<void> {
  const id = ++seq
  const t0 = performance.now()

  if (inFlight) {
    publishProbe({
      timestamp: new Date().toISOString(),
      level: 'probe',
      seq: id,
      trigger,
      success: false,
      targetApp: null,
      processName: null,
      processId: null,
      windowTitle: null,
      focusedElement: null,
      text: null,
      rectCount: 0,
      rects: [],
      elapsedMs: Math.round(performance.now() - t0),
      nativeElapsedMs: 0,
      errorCode: 'Busy',
      errorMessage: 'previous probe still in flight — request skipped'
    })
    return
  }
  inFlight = true

  let result: ProbeResult
  try {
    result = await probeSelectionDetailed(config.timeoutMs)
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
  publishProbe({
    timestamp: new Date().toISOString(),
    level: 'probe',
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

// ---- panel window ----------------------------------------------------------

function createWindow(): void {
  win = new BrowserWindow({
    width: 440,
    height: 640,
    minWidth: 360,
    title: 'UIA 划词探测',
    alwaysOnTop: config.alwaysOnTop,
    webPreferences: {
      preload: join(__dirname, '../preload/index.js')
    }
  })

  win.on('close', (e) => {
    // Closing hides the panel; the process and hotkeys stay alive.
    if (!quitting) {
      e.preventDefault()
      win?.hide()
    }
  })

  if (process.env['ELECTRON_RENDERER_URL']) {
    void win.loadURL(process.env['ELECTRON_RENDERER_URL'])
  } else {
    void win.loadFile(join(__dirname, '../renderer/index.html'))
  }
}

function togglePanel(): void {
  if (!win) return
  if (win.isVisible() && win.isFocused()) {
    win.hide()
  } else {
    win.show()
    win.focus()
  }
}

// ---- hotkey registration / config -------------------------------------------

function registerHotkeys(): { hotkey: boolean; panelHotkey: boolean } {
  return {
    hotkey: globalShortcut.register(config.hotkey, () => void handleProbe('hotkey')),
    panelHotkey: globalShortcut.register(config.panelHotkey, () => togglePanel())
  }
}

function applyConfig(next: AppConfig): SetConfigResult {
  if (next.hotkey === next.panelHotkey) {
    return { ok: false, config, error: '划词热键与面板热键不能相同' }
  }
  const prev = config
  config = next

  // Re-register only the changed hotkey so an unrelated conflict
  // (e.g. one key held by another app since startup) doesn't block saves.
  const errors: string[] = []
  const reregister = (key: 'hotkey' | 'panelHotkey', label: string): void => {
    if (next[key] === prev[key]) return
    globalShortcut.unregister(prev[key])
    const action = key === 'hotkey' ? (): void => void handleProbe('hotkey') : (): void => togglePanel()
    if (!globalShortcut.register(config[key], action)) {
      errors.push(`${label}注册失败（可能被其他应用占用）`)
      config[key] = prev[key]
      globalShortcut.register(prev[key], action)
    }
  }
  reregister('hotkey', '划词热键')
  reregister('panelHotkey', '面板热键')

  win?.setAlwaysOnTop(config.alwaysOnTop)
  saveConfig(config)
  emit('info', { msg: 'config updated', config })
  return errors.length === 0
    ? { ok: true, config }
    : { ok: false, config, error: errors.join('；') }
}

// ---- IPC ---------------------------------------------------------------------

ipcMain.handle('probe:getState', () => ({ latest: history[0] ?? null, history }))
ipcMain.handle('config:get', () => config)
ipcMain.handle('config:set', (_e, patch: Partial<AppConfig>) => {
  const clamp = (v: unknown, fallback: number): number => {
    const n = Number(v)
    return Number.isFinite(n) ? Math.min(10000, Math.max(100, Math.round(n))) : fallback
  }
  return applyConfig({
    hotkey: typeof patch.hotkey === 'string' && patch.hotkey ? patch.hotkey : config.hotkey,
    panelHotkey: typeof patch.panelHotkey === 'string' && patch.panelHotkey ? patch.panelHotkey : config.panelHotkey,
    timeoutMs: patch.timeoutMs !== undefined ? clamp(patch.timeoutMs, config.timeoutMs) : config.timeoutMs,
    alwaysOnTop: patch.alwaysOnTop ?? config.alwaysOnTop
  })
})
ipcMain.handle('probe:trigger', () => void handleProbe('manual'))
ipcMain.on('panel:toggle', () => togglePanel())

app.disableHardwareAcceleration()

app.whenReady().then(() => {
  createWindow()

  const registered = registerHotkeys()

  emit('info', {
    msg: 'UIA selection probe ready',
    hotkey: config.hotkey,
    hotkeyRegistered: registered.hotkey,
    panelHotkey: config.panelHotkey,
    panelHotkeyRegistered: registered.panelHotkey,
    timeoutMs: config.timeoutMs,
    logFile: LOG_FILE,
    electron: process.versions.electron,
    node: process.versions.node
  })

  if (!registered.hotkey) {
    emit('error', { msg: `failed to register hotkey ${config.hotkey} (conflict with another app?)` })
  }
  if (!registered.panelHotkey) {
    emit('error', { msg: `failed to register panel hotkey ${config.panelHotkey} (conflict with another app?)` })
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

app.on('before-quit', () => {
  quitting = true
})

app.on('will-quit', () => globalShortcut.unregisterAll())
app.on('window-all-closed', () => {
  // Panel may be hidden; keep the process alive for hotkey probing.
})
