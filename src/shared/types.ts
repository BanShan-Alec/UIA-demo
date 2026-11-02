import type { FocusInfo, SelectionRect } from 'uia-probe'

/** One probe record — mirrors the `probe` line written to stdout / uia-probe.log. */
export interface ProbeRecord {
  timestamp: string
  level: 'probe'
  seq: number
  trigger: 'hotkey' | 'selftest' | 'manual'
  success: boolean
  targetApp: string | null
  processName: string | null
  processId: number | null
  windowTitle: string | null
  focusedElement: FocusInfo | null
  text: string | null
  rectCount: number
  rects: SelectionRect[]
  elapsedMs: number
  nativeElapsedMs: number
  errorCode: string | null
  errorMessage: string | null
  warnings?: string[]
}

export interface AppConfig {
  /** Text-selection hotkey (Electron accelerator, e.g. Control+Shift+2). */
  hotkey: string
  /** Panel show/hide hotkey. */
  panelHotkey: string
  /** Probe timeout in milliseconds. */
  timeoutMs: number
  /** Keep the panel window always on top. */
  alwaysOnTop: boolean
}

export interface SetConfigResult {
  ok: boolean
  config: AppConfig
  error?: string
}

/** Payload passed from the native Win32 dot to the result panel. */
export interface DotSelection {
  text: string
  rects: SelectionRect[]
  processName: string
  paragraph: string | null
  paragraphError: string | null
  /** Dot window rectangle in absolute physical pixels. */
  dotRect: SelectionRect
}
