import './style.css'
import type { PanelApi } from '../../preload/index'
import type { AppConfig, ProbeRecord, SetConfigResult } from '../../shared/types'

declare global {
  interface Window {
    api: PanelApi
  }
}

const HISTORY_CAP = 20

let history: ProbeRecord[] = []
let selectedSeq: number | null = null
let config: AppConfig | null = null

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T

const banner = $('banner')
const detail = $('detail')
const historyEl = $('history')
const saveStatus = $('save-status')
const hotkeyInput = $<HTMLInputElement>('hotkey')
const panelHotkeyInput = $<HTMLInputElement>('panel-hotkey')
const timeoutInput = $<HTMLInputElement>('timeout')
const onTopInput = $<HTMLInputElement>('on-top')

function fmtTime(iso: string): string {
  return new Date(iso).toLocaleTimeString('zh-CN', { hour12: false })
}

function renderDetail(record: ProbeRecord): void {
  const status = record.success
    ? `成功 · seq ${record.seq} · ${fmtTime(record.timestamp)}`
    : `失败 · ${record.errorCode} · seq ${record.seq} · ${fmtTime(record.timestamp)}`
  banner.className = `banner ${record.success ? 'ok' : 'err'}`
  banner.textContent = status

  const rows: Array<[string, string]> = [
    ['目标应用', record.targetApp ?? '—'],
    ['触发方式', record.trigger],
    ['聚焦元素', record.focusedElement ? `${record.focusedElement.controlType} "${record.focusedElement.name}" (via ${record.focusedElement.via})` : '—'],
    ['耗时', `${record.elapsedMs} ms（原生 ${record.nativeElapsedMs} ms）`],
    ['错误信息', record.errorMessage ?? '—']
  ]
  if (record.warnings?.length) rows.push(['警告', record.warnings.join('；')])

  const dl = document.createElement('dl')
  dl.className = 'meta'
  for (const [k, v] of rows) {
    const dt = document.createElement('dt')
    dt.textContent = k
    const dd = document.createElement('dd')
    dd.textContent = v
    dl.append(dt, dd)
  }

  const pre = document.createElement('pre')
  pre.textContent = record.text ?? '<无文本>'

  detail.className = 'detail'
  detail.replaceChildren()
  if (record.text !== null) detail.append('划词内容：')
  detail.append(pre, dl)

  if (record.rects.length > 0) {
    const ul = document.createElement('ul')
    ul.className = 'rects'
    for (const r of record.rects) {
      const li = document.createElement('li')
      li.textContent = `rect(${r.x}, ${r.y}, ${r.width}, ${r.height})`
      ul.append(li)
    }
    detail.append(`矩形区域 ×${record.rectCount}：`, ul)
  }
}

function renderHistory(): void {
  historyEl.replaceChildren()
  if (history.length === 0) {
    const p = document.createElement('p')
    p.className = 'history-empty'
    p.textContent = '暂无记录'
    historyEl.append(p)
    return
  }
  for (const record of history) {
    const li = document.createElement('li')
    if (record.seq === selectedSeq) li.className = 'current'

    const t = document.createElement('span')
    t.className = 't'
    t.textContent = fmtTime(record.timestamp)
    const app = document.createElement('span')
    app.className = 'app'
    app.textContent = record.targetApp ?? record.processName ?? '未知应用'
    const flag = document.createElement('span')
    if (record.success) {
      flag.className = 'ok'
      flag.textContent = '✓'
    } else {
      flag.className = 'err'
      flag.textContent = record.errorCode ?? '失败'
    }
    li.append(t, app, flag)
    li.addEventListener('click', () => {
      selectedSeq = record.seq
      renderDetail(record)
      renderHistory()
    })
    historyEl.append(li)
  }
}

function applyRecord(record: ProbeRecord, select: boolean): void {
  history = [record, ...history.filter((r) => r.seq !== record.seq)].slice(0, HISTORY_CAP)
  if (select) selectedSeq = record.seq
  if (select) renderDetail(record)
  renderHistory()
}

// ---- hotkey capture -------------------------------------------------------

const MODIFIERS: Array<[string, 'ctrlKey' | 'altKey' | 'shiftKey' | 'metaKey']> = [
  ['Control', 'ctrlKey'],
  ['Alt', 'altKey'],
  ['Shift', 'shiftKey'],
  ['Super', 'metaKey']
]

function keyToAccelerator(e: KeyboardEvent): string | null {
  const parts = MODIFIERS.filter(([, prop]) => e[prop]).map(([name]) => name)
  let key: string | null = null
  if (/^Key[A-Z]$/.test(e.code)) key = e.code.slice(3)
  else if (/^Digit\d$/.test(e.code)) key = e.code.slice(5)
  else if (/^F\d{1,2}$/.test(e.code)) key = e.code
  else if (/^(ArrowUp|ArrowDown|ArrowLeft|ArrowRight|Home|End|PageUp|PageDown)$/.test(e.code)) key = e.code
  if (!key || (parts.length === 0 && !/^F\d{1,2}$/.test(e.code))) return null
  return [...parts, key].join('+')
}

function wireHotkeyCapture(input: HTMLInputElement): void {
  input.addEventListener('focus', () => input.classList.add('capturing'))
  input.addEventListener('blur', () => input.classList.remove('capturing'))
  input.addEventListener('keydown', (e) => {
    e.preventDefault()
    e.stopPropagation()
    if (e.key === 'Escape') {
      input.blur()
      return
    }
    const accel = keyToAccelerator(e)
    if (accel) {
      input.value = accel
      input.blur()
    }
  })
}

// ---- config ---------------------------------------------------------------

function fillConfigForm(cfg: AppConfig): void {
  hotkeyInput.value = cfg.hotkey
  panelHotkeyInput.value = cfg.panelHotkey
  timeoutInput.value = String(cfg.timeoutMs)
  onTopInput.checked = cfg.alwaysOnTop
}

function flashSaveStatus(result: SetConfigResult): void {
  saveStatus.className = `save-status ${result.ok ? 'ok' : 'err'}`
  saveStatus.textContent = result.ok ? '已保存' : `保存失败：${result.error ?? '未知错误'}`
  setTimeout(() => {
    saveStatus.textContent = ''
    saveStatus.className = 'save-status'
  }, 4000)
  if (result.ok) config = result.config
  fillConfigForm(result.config)
}

$('probe-now').addEventListener('click', () => void window.api.triggerProbe())

$('save-config').addEventListener('click', async () => {
  const patch: Partial<AppConfig> = {
    hotkey: hotkeyInput.value,
    panelHotkey: panelHotkeyInput.value,
    timeoutMs: Number(timeoutInput.value) || 500,
    alwaysOnTop: onTopInput.checked
  }
  flashSaveStatus(await window.api.setConfig(patch))
})

wireHotkeyCapture(hotkeyInput)
wireHotkeyCapture(panelHotkeyInput)

// ---- init -----------------------------------------------------------------

void window.api.getState().then(({ latest, history: initial }) => {
  history = initial
  if (latest) applyRecord(latest, true)
  renderHistory()
})

void window.api.getConfig().then((cfg) => {
  config = cfg
  fillConfigForm(cfg)
})

void window.api.onProbeResult((record) => applyRecord(record, true))
