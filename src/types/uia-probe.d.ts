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

  /** One `ExpandToEnclosingUnit` measurement. */
  export interface UnitProbeResult {
    /** Character / Format / Word / Line / Paragraph / Page / Document */
    unit: string
    unitValue: number
    /** "selection" = live selection as-is; "collapsed-start" = collapsed to its start. */
    mode: string
    /** Character count of the expanded range before transport truncation. */
    textChars: number
    /** First ~160 chars with line breaks escaped, for logs. */
    textPreview: string
    /** Expanded range text (truncated to 5000 chars). */
    text: string
    textTruncated: boolean
    rectCount: number
    /** True when the expanded range covers the whole document (degradation signal). */
    equalsDocument: boolean
    warnings: string[]
  }

  /** Audit-only result of probeExpandUnits. */
  export interface ExpandProbeResult {
    success: boolean
    processName: string
    processId: number
    windowTitle: string
    focusedElement: FocusInfo | null
    selectionRanges: number
    selectionChars: number
    selectionText: string
    /** DocumentRange text length — baseline for the degradation check. */
    documentChars: number
    documentPreview: string
    results: UnitProbeResult[]
    nativeElapsedMs: number
    errorCode: string | null
    errorMessage: string | null
    warnings: string[]
  }

  /** Event emitted when the native dot is clicked. */
  export interface DotSelectionEvent {
    text: string
    rects: SelectionRect[]
    processName: string
    paragraph: string | null
    paragraphError: string | null
    /** Dot window rectangle in absolute physical pixels. */
    dotRect: SelectionRect
  }

  /** Product call. errorCode null = success. */
  export function probeSelection(timeoutMs?: number): Promise<SelectionResult>
  /** Audit call (full fields) — used by the PoC log path. */
  export function probeSelectionDetailed(timeoutMs?: number): Promise<ProbeResult>
  /** Audit call: ExpandToEnclosingUnit measurements for the live selection.
   *  `windowTitle` is automation-only: find a visible window by title substring
   *  instead of using GetForegroundWindow. */
  export function probeExpandUnits(
    timeoutMs?: number,
    windowTitle?: string
  ): Promise<ExpandProbeResult>
  /** Start the native Win32 dot thread. The callback runs once per dot click. */
  export function startDot(callback: (event: DotSelectionEvent) => void): void
  /** Stop the native dot thread (call on app quit). */
  export function stopDot(): void
}
