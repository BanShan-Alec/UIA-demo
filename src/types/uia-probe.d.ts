declare module 'uia-probe' {
  /** Per-line selection bounding box, absolute physical pixels. */
  export interface SelectionRect {
    x: number
    y: number
    width: number
    height: number
  }

  /**
   * Product interface (4 fields): what to translate, where to place the
   * popup, what went wrong, which app the selection came from.
   */
  export interface SelectionResult {
    text: string
    rects: SelectionRect[]
    errorCode: string | null
    processName: string
  }

  /** Focus context of the probe (audit variant only). */
  export interface FocusInfo {
    name: string
    controlType: string
    via: string
    processId: number
  }

  /** Full audit variant — PoC log path only; product code should use SelectionResult. */
  export interface ProbeResult {
    success: boolean
    processName: string
    processId: number
    windowTitle: string
    focusedElement: FocusInfo | null
    text: string | null
    rectCount: number
    rects: SelectionRect[]
    nativeElapsedMs: number
    errorCode: string | null
    errorMessage: string | null
    warnings: string[]
  }

  /** Product call. errorCode null = success. */
  export function probeSelection(timeoutMs?: number): Promise<SelectionResult>
  /** Audit call (full fields) — used by the PoC log path. */
  export function probeSelectionDetailed(timeoutMs?: number): Promise<ProbeResult>
}
