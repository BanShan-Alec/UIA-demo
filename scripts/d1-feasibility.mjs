#!/usr/bin/env node
/**
 * D-1 feasibility run: 3 page types x (5 words + 5 sentences) = 30
 * deterministic selections, plus Q-6 RangeFromPoint at the same points.
 *
 * Page types (Snapword PRD D-1):
 *   1. ordinary article  — English Wikipedia
 *   2. GitHub README     — repository landing page
 *   3. page with iframe  — local parent + child document
 *
 * The browser is launched with --force-renderer-accessibility, a CDP focus
 * emulation, and a real click inside the target document. Those three steps
 * are required for Chromium's UIA provider to expose the web Document.
 *
 * Usage:
 *   node scripts/d1-feasibility.mjs
 *   node scripts/d1-feasibility.mjs --browser edge
 *   node scripts/d1-feasibility.mjs --pages article,github --words 5 --sentences 5
 */
import { execFileSync, spawn } from 'node:child_process'
import { createServer } from 'node:http'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const require = createRequire(import.meta.url)
const native = require('../native/index.js')
const root = dirname(dirname(fileURLToPath(import.meta.url)))

// ---- args -----------------------------------------------------------------

const argv = process.argv.slice(2)
const arg = (name, fallback = null) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback
}
const flag = (name) => argv.includes(`--${name}`)

const browserName = arg('browser', 'chrome')
const wordsPerPage = Number(arg('words', '5'))
const sentencesPerPage = Number(arg('sentences', '5'))
const probeTimeout = Number(arg('timeout', '2000'))
const probeMode = arg('probe-mode', 'title') // "title" (automation-safe) or "foreground"
const reportPath = resolve(root, arg('report', 'd1-probe-report.json'))
const keepOpen = flag('keep-open')
const pageFilter = new Set(
  arg('pages', 'article,github,iframe')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
)

const browserExe =
  browserName === 'chrome'
    ? 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe'
    : 'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe'

if (!existsSync(browserExe)) {
  console.error(`browser not found: ${browserExe}`)
  process.exit(2)
}

// ---- tiny CDP client ------------------------------------------------------

class Cdp {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl)
    this.nextId = 0
    this.pending = new Map()
    this.ws.onmessage = (event) => this.onMessage(event.data)
  }

  open() {
    return new Promise((resolve, reject) => {
      this.ws.onopen = resolve
      this.ws.onerror = (event) => reject(new Error(`CDP socket error: ${event.message ?? 'unknown'}`))
    })
  }

  send(method, params = {}) {
    const id = ++this.nextId
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      this.ws.send(JSON.stringify({ id, method, params }))
    })
  }

  onMessage(raw) {
    const message = JSON.parse(raw)
    if (!message.id) return
    const pending = this.pending.get(message.id)
    if (!pending) return
    this.pending.delete(message.id)
    if (message.error) pending.reject(new Error(`${message.error.message} (${message.error.code})`))
    else pending.resolve(message.result)
  }

  close() {
    try {
      this.ws.close()
    } catch {
      /* already closed */
    }
  }
}

async function sleep(ms) {
  await new Promise((resolve) => setTimeout(resolve, ms))
}

async function waitFor(fn, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs
  let lastError = null
  while (Date.now() < deadline) {
    try {
      const value = await fn()
      if (value) return value
    } catch (error) {
      lastError = error
    }
    await sleep(200)
  }
  throw new Error(`timeout waiting for ${label}${lastError ? `: ${lastError.message}` : ''}`)
}

function activateWindow(pid) {
  try {
    execFileSync(
      'powershell',
      ['-NoProfile', '-Command', `(New-Object -ComObject WScript.Shell).AppActivate(${pid})`],
      { stdio: 'ignore', timeout: 5000 }
    )
  } catch {
    /* best effort; Page.bringToFront usually suffices */
  }
}

// ---- clipboard sequence ---------------------------------------------------

const CLIPBOARD_SCRIPT = `
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public class ClipSeq { [DllImport("user32.dll")] public static extern uint GetClipboardSequenceNumber(); }';
[ClipSeq]::GetClipboardSequenceNumber()
`

function clipboardSequence() {
  try {
    const output = execFileSync('powershell', ['-NoProfile', '-Command', CLIPBOARD_SCRIPT], {
      encoding: 'utf8',
      timeout: 15000
    })
    const lines = output.trim().split(/\r?\n/).filter(Boolean)
    const value = Number(lines[lines.length - 1])
    return Number.isFinite(value) ? value : null
  } catch {
    return null
  }
}

// ---- local static server (iframe page) ------------------------------------

function mimeType(filePath) {
  if (filePath.endsWith('.html')) return 'text/html; charset=utf-8'
  if (filePath.endsWith('.css')) return 'text/css; charset=utf-8'
  if (filePath.endsWith('.js')) return 'text/javascript; charset=utf-8'
  if (filePath.endsWith('.pdf')) return 'application/pdf'
  return 'application/octet-stream'
}

function startStaticServer(directory) {
  const server = createServer((req, res) => {
    const pathname = decodeURIComponent(new URL(req.url, 'http://127.0.0.1').pathname)
    const relative = pathname === '/' ? 'index.html' : pathname.replace(/^\/+/, '')
    const filePath = resolve(directory, relative)
    if (!filePath.startsWith(resolve(directory))) {
      res.writeHead(403)
      res.end('forbidden')
      return
    }
    try {
      const body = readFileSync(filePath)
      res.writeHead(200, { 'Content-Type': mimeType(filePath) })
      res.end(body)
    } catch {
      res.writeHead(404)
      res.end('not found')
    }
  })
  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => {
      resolveServer({ server, port: server.address().port })
    })
  })
}

// ---- in-page selection helper --------------------------------------------

const PAGE_HELPER = String.raw`
(() => {
  const eligible = (doc, selector) => Array.from(doc.querySelectorAll(selector)).filter((el) => {
    const text = (el.textContent || '').trim();
    return text.length >= 80 && /[A-Za-z]{3}/.test(text) && !el.closest('pre, code');
  });

  const textNodes = (doc, root) => {
    const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    const nodes = [];
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (n.data.length > 0) nodes.push(n);
    }
    return nodes;
  };

  const pointAt = (nodes, offset) => {
    let remain = offset;
    for (const node of nodes) {
      if (remain <= node.data.length) return { node, offset: remain };
      remain -= node.data.length;
    }
    const last = nodes[nodes.length - 1];
    return { node: last, offset: last.data.length };
  };

  const locate = (doc, root, needle, occurrence = 0) => {
    const nodes = textNodes(doc, root);
    let seen = 0;
    for (const node of nodes) {
      let from = 0;
      let index;
      while ((index = node.data.indexOf(needle, from)) !== -1) {
        if (seen === occurrence) {
          return { startNode: node, startOffset: index, endNode: node, endOffset: index + needle.length };
        }
        seen += 1;
        from = index + needle.length;
      }
    }
    const joined = nodes.map((n) => n.data).join('');
    seen = 0;
    let from = 0;
    let index;
    while ((index = joined.indexOf(needle, from)) !== -1) {
      if (seen === occurrence) {
        const start = pointAt(nodes, index);
        const end = pointAt(nodes, index + needle.length);
        return { startNode: start.node, startOffset: start.offset, endNode: end.node, endOffset: end.offset };
      }
      seen += 1;
      from = index + needle.length;
    }
    return null;
  };

  const apply = (doc, startNode, startOffset, endNode, endOffset) => {
    const range = doc.createRange();
    range.setStart(startNode, startOffset);
    range.setEnd(endNode, endOffset);
    const selection = doc.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    const clientRects = Array.from(range.getClientRects());
    const first = clientRects[0] ?? range.getBoundingClientRect();
    return {
      selected: selection.toString(),
      rect: { x: first.x, y: first.y, width: first.width, height: first.height }
    };
  };

  const select = (doc, el, mode) => {
    const text = (el.textContent || '').trim();
    // Real users only select visible text. Scroll the target block into view
    // so the UIA bounding rect is on-screen and RangeFromPoint can hit it.
    el.scrollIntoView({ block: 'center' });
    let target;
    if (mode === 'word') {
      const words = text.match(/[A-Za-z][A-Za-z'-]{2,}/g);
      if (!words) return { error: 'no ascii word' };
      target = words[0].replace(/[^A-Za-z]+$/, '') || words[0];
    } else {
      const sentence = text.match(/[A-Z][^.!?]{20,240}[.!?]/);
      target = sentence ? sentence[0] : text.slice(0, 160);
    }
    const hit = locate(doc, el, target);
    if (!hit) return { error: 'text not found: ' + target };
    const applied = apply(doc, hit.startNode, hit.startOffset, hit.endNode, hit.endOffset);
    return { selected: applied.selected, rect: applied.rect, full: text, target, mode };
  };

  window.__d1 = {
    list(doc, selector) {
      return eligible(doc, selector).map((el, i) => ({ i, chars: (el.textContent || '').trim().length }));
    },
    select(doc, selector, index, mode) {
      const els = eligible(doc, selector);
      const el = els[index];
      if (!el) return { error: 'no eligible element at index ' + index };
      return select(doc, el, mode);
    },
    firstRect(doc, selector) {
      const el = eligible(doc, selector)[0];
      if (!el) return null;
      el.scrollIntoView({ block: 'center' });
      const r = el.getBoundingClientRect();
      return { x: Math.round(r.left + Math.min(20, r.width / 2)), y: Math.round(r.top + Math.min(10, r.height / 2)) };
    },
    iframeDoc() {
      const frame = document.querySelector('iframe');
      if (!frame) return null;
      const doc = frame.contentDocument;
      if (!doc || doc.readyState !== 'complete') return null;
      return doc;
    },
    iframeRect() {
      const frame = document.querySelector('iframe');
      if (!frame) return null;
      frame.scrollIntoView({ block: 'center' });
      const r = frame.getBoundingClientRect();
      return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + 40) };
    },
    iframeBox() {
      const frame = document.querySelector('iframe');
      if (!frame) return null;
      const r = frame.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, height: r.height };
    },
    chromeBox() {
      return { x: (outerWidth - innerWidth) / 2, y: outerHeight - innerHeight };
    }
  };
})();
`

// ---- main -----------------------------------------------------------------

const pages = [
  {
    name: 'article',
    label: '普通文章页（Wikipedia）',
    url: 'https://en.wikipedia.org/wiki/UI_Automation',
    selectors: ['#mw-content-text p', '#mw-content-text p, #mw-content-text li'],
    windowTitle: 'UI Automation - Wikipedia',
    inIframe: false
  },
  {
    name: 'github',
    label: 'GitHub README（microsoft/vscode）',
    url: 'https://github.com/microsoft/vscode',
    selectors: [
      'article.markdown-body p, article.markdown-body li',
      'article.markdown-body p, article.markdown-body li, article.markdown-body blockquote'
    ],
    windowTitle: 'vscode',
    inIframe: false
  },
  {
    name: 'iframe',
    label: '含 iframe 的本地页面',
    url: null, // filled after the static server starts
    selectors: ['#f1, #f2, #f3, #f4, #f5, #f6'],
    windowTitle: 'UIA-IFRAME-TEST',
    inIframe: true
  }
].filter((page) => pageFilter.has(page.name))

const userDataDir = mkdtempSync(join(tmpdir(), 'uia-d1-'))
let child = null
let cdp = null
let staticServer = null

const report = {
  generatedAt: new Date().toISOString(),
  browser: browserName,
  browserVersion: null,
  wordsPerPage,
  sentencesPerPage,
  clipboardBefore: null,
  clipboardAfter: null,
  clipboardWrites: null,
  pages: [],
  summary: null,
  failures: []
}

const failures = []
const check = (scope, name, ok, detail) => {
  if (!ok) failures.push(`${scope} / ${name}: ${detail}`)
}

const norm = (value) =>
  String(value ?? '')
    .replace(/\r\n/g, '\n')
    .replace(/\s+/g, ' ')
    .trim()

const probeSelection = (timeout, title) =>
  probeMode === 'foreground'
    ? native.probeSelectionDetailed(timeout)
    : native.probeSelectionDetailed(timeout, title)

const probeRange = (x, y, timeout, title) =>
  probeMode === 'foreground'
    ? native.probeRangeFromPoint(x, y, timeout)
    : native.probeRangeFromPoint(x, y, timeout, title)

const percentile = (values, p) => {
  if (values.length === 0) return null
  const sorted = [...values].sort((a, b) => a - b)
  const index = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1)
  return sorted[index]
}

try {
  report.clipboardBefore = clipboardSequence()

  const local = await startStaticServer(join(root, 'testpage'))
  staticServer = local.server
  const iframePage = pages.find((page) => page.name === 'iframe')
  if (iframePage) iframePage.url = `http://127.0.0.1:${local.port}/iframe.html`

  console.log(`[d1] launching ${browserName}: ${browserExe}`)
  child = spawn(
    browserExe,
    [
      `--user-data-dir=${userDataDir}`,
      '--remote-debugging-port=0',
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-background-networking',
      '--disable-sync',
      '--disable-extensions',
      '--start-maximized',
      ...(browserName === 'edge' ? ['--guest', '--enable-features=UiaProvider'] : []),
      '--force-renderer-accessibility',
      pages[0]?.url ?? 'about:blank'
    ],
    { stdio: 'ignore' }
  )

  const portFile = join(userDataDir, 'DevToolsActivePort')
  await waitFor(() => existsSync(portFile), 20000, 'DevToolsActivePort')
  const [portLine] = readFileSync(portFile, 'utf8').trim().split(/\r?\n/)
  const port = Number(portLine)
  if (!port) throw new Error(`invalid DevTools port file: ${portLine}`)

  const version = await fetch(`http://127.0.0.1:${port}/json/version`).then((r) => r.json())
  report.browserVersion = version.Browser
  console.log(`[d1] ${version.Browser}`)

  const targets = await waitFor(
    async () => {
      const list = await fetch(`http://127.0.0.1:${port}/json/list`).then((r) => r.json())
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      return page ? list : null
    },
    20000,
    'page target'
  )
  const target =
    targets.find((t) => t.type === 'page' && t.url && t.url !== 'about:blank') ??
    targets.find((t) => t.type === 'page')
  if (!target?.webSocketDebuggerUrl) throw new Error('no debuggable page target')

  cdp = new Cdp(target.webSocketDebuggerUrl)
  await cdp.open()
  await cdp.send('Page.enable')
  await cdp.send('Runtime.enable')
  try {
    await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true })
  } catch {
    /* older Chromium */
  }

  const evaluate = async (expression) => {
    const result = await cdp.send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true
    })
    if (result.exceptionDetails) {
      throw new Error(result.exceptionDetails.exception?.description ?? 'page evaluation failed')
    }
    return result.result.value
  }

  const clickAt = async (point) => {
    if (!point) return
    for (const type of ['mousePressed', 'mouseReleased']) {
      await cdp.send('Input.dispatchMouseEvent', {
        type,
        x: point.x,
        y: point.y,
        button: 'left',
        clickCount: 1
      })
    }
  }

  const runPage = async (page) => {
    console.log(`\n[d1] === ${page.label} ===`)
    await cdp.send('Page.navigate', { url: page.url })

    await waitFor(
      async () =>
        (await evaluate(
          page.inIframe
            ? `!!document.querySelector('iframe')`
            : `!!document.querySelector(${JSON.stringify(page.selectors[0])})`
        )) === true,
      45000,
      `${page.name} selector`
    )
    if (page.inIframe) {
      await waitFor(
        async () =>
          (await evaluate(
            `!!(document.querySelector('iframe') && document.querySelector('iframe').contentDocument && document.querySelector('iframe').contentDocument.querySelector('#f1'))`
          )) === true,
        30000,
        'iframe child document'
      )
    }
    await evaluate(PAGE_HELPER)

    // Pick the first selector that yields enough eligible paragraphs.
    let selector = page.selectors[0]
    let count = 0
    for (const candidate of page.selectors) {
      const list = await evaluate(
        `window.__d1.list(${page.inIframe ? "window.__d1.iframeDoc()" : 'document'}, ${JSON.stringify(candidate)})`
      )
      if (Array.isArray(list) && list.length >= Math.max(wordsPerPage, sentencesPerPage)) {
        selector = candidate
        count = list.length
        break
      }
      count = Math.max(count, Array.isArray(list) ? list.length : 0)
    }
    if (count < Math.max(wordsPerPage, sentencesPerPage)) {
      throw new Error(`${page.name}: only ${count} eligible blocks, need ${Math.max(wordsPerPage, sentencesPerPage)}`)
    }
    console.log(`[d1] selector ${selector} -> ${count} eligible blocks`)

    // Click inside the target document once so Chromium marks it focused.
    const clickPoint = page.inIframe
      ? await evaluate('window.__d1.iframeRect()')
      : await evaluate(`window.__d1.firstRect(document, ${JSON.stringify(selector)})`)
    await clickAt(clickPoint)
    await sleep(300)

    const pageTitle = await evaluate('document.title')
    const titleNeedle = page.windowTitle || pageTitle
    const dpr = await evaluate('devicePixelRatio')
    const chromeBox = await evaluate('window.__d1.chromeBox()')
    const iframeBox = page.inIframe ? await evaluate('window.__d1.iframeBox()') : null

    // Warm-up: the first UIA tree enumeration activates Chromium's full
    // accessibility tree. Do it before recording any sample.
    await evaluate(
      `window.__d1.select(${page.inIframe ? "window.__d1.iframeDoc()" : 'document'}, ${JSON.stringify(selector)}, 0, 'word')`
    )
    await cdp.send('Page.bringToFront')
    activateWindow(child.pid)
    await sleep(300)
    await probeSelection(probeTimeout, titleNeedle)
    await sleep(600)

    if (flag('debug') && page.inIframe) {
      const geom = await evaluate(`(() => {
        const f = document.querySelector('iframe');
        const fr = f.getBoundingClientRect();
        const doc = f.contentDocument;
        const el = doc.querySelector('#f1');
        const r = el.getBoundingClientRect();
        const sel = doc.getSelection();
        const sr = sel.rangeCount ? sel.getRangeAt(0).getBoundingClientRect() : null;
        return {
          dpr: devicePixelRatio,
          screenX, screenY, outerWidth, outerHeight, innerWidth, innerHeight,
          iframe: { x: fr.x, y: fr.y, w: fr.width, h: fr.height },
          f1: { x: r.x, y: r.y, w: r.width, h: r.height },
          sel: sr ? { x: sr.x, y: sr.y, w: sr.width, h: sr.height } : null
        };
      })()`)
      console.log('[d1][debug]', JSON.stringify(geom))
    }

    const samples = []
    const modes = [
      { mode: 'word', count: wordsPerPage },
      { mode: 'sentence', count: sentencesPerPage }
    ]

    for (const { mode, count: modeCount } of modes) {
      for (let index = 0; index < modeCount; index += 1) {
        const selection = await evaluate(
          `window.__d1.select(${page.inIframe ? "window.__d1.iframeDoc()" : 'document'}, ${JSON.stringify(selector)}, ${index}, ${JSON.stringify(mode)})`
        )
        if (selection?.error) {
          const sample = { mode, index, selection, error: selection.error }
          samples.push(sample)
          failures.push(`${page.name}/${mode}[${index}] selection: ${selection.error}`)
          continue
        }

        // The DOM selection rect is viewport CSS. Convert it to screen CSS by
        // adding the iframe offset (if any) and the browser chrome height.
        let screenCssRect = null
        if (selection.rect) {
          screenCssRect = { ...selection.rect }
          if (page.inIframe && iframeBox) {
            screenCssRect.x += iframeBox.x
            screenCssRect.y += iframeBox.y
          }
          if (chromeBox) {
            screenCssRect.x += chromeBox.x
            screenCssRect.y += chromeBox.y
          }
        }

        await cdp.send('Page.bringToFront')
        activateWindow(child.pid)
        await sleep(400)

        const probe = await probeSelection(probeTimeout, titleNeedle)
        const selectedNorm = norm(selection.selected)
        const textNorm = norm(probe.text ?? '')
        const textMatch = probe.success === true && textNorm === selectedNorm
        const rects = Array.isArray(probe.rects) ? probe.rects : []
        const usableRects = rects.filter((r) => r.width > 1 && r.height > 1)
        const rectOk = usableRects.length > 0

        // Chromium returns iframe text rects in screen CSS pixels, while
        // top-level document rects are physical pixels. Compare the UIA rect
        // with both interpretations so Q-6 uses the right point space.
        let rectSpace = null
        let cssDiff = null
        let physicalDiff = null
        if (usableRects[0] && screenCssRect) {
          const uia = usableRects[0]
          cssDiff =
            Math.abs(uia.x - screenCssRect.x) + Math.abs(uia.y - screenCssRect.y)
          physicalDiff =
            Math.abs(uia.x - screenCssRect.x * dpr) +
            Math.abs(uia.y - screenCssRect.y * dpr)
          rectSpace = cssDiff <= physicalDiff ? 'css' : 'physical'
        }

        const sample = {
          mode,
          index,
          selection: { selected: selection.selected, target: selection.target },
          probe: {
            success: probe.success,
            errorCode: probe.errorCode ?? null,
            errorMessage: probe.errorMessage ?? null,
            text: probe.text ?? '',
            textChars: probe.text ? probe.text.length : 0,
            rectCount: rects.length,
            usableRectCount: usableRects.length,
            nativeElapsedMs: probe.nativeElapsedMs,
            focusedVia: probe.focusedElement?.via ?? null,
            focusedControlType: probe.focusedElement?.controlType ?? null,
            focusedName: probe.focusedElement?.name ?? null,
            rects: rects.map((r) => ({ x: r.x, y: r.y, width: r.width, height: r.height }))
          },
          screenCssRect,
          rectSpace,
          cssDiff,
          physicalDiff,
          textMatch,
          rectOk,
          q6: null
        }

        if (textMatch && rectOk) {
          const uiaRect = usableRects[0]
          // Use the UIA rect as the point source: it is measured at probe
          // time, while the DOM rect can be stale after the page scrolls.
          // The DOM rect is only used above to infer the rect coordinate
          // space (CSS vs physical).
          const base = uiaRect
          const raw = {
            x: mode === 'word' ? base.x + base.width / 2 : base.x + Math.min(8, base.width / 2),
            y: base.y + base.height * 0.7
          }
          if (flag('debug')) {
            const candidates = [
              { space: 'raw-css', x: raw.x, y: raw.y },
              { space: 'physical', x: raw.x * dpr, y: raw.y * dpr }
            ]
            for (const candidate of candidates) {
              const r = await probeRange(candidate.x, candidate.y, probeTimeout, titleNeedle)
              console.log(
                `[d1][debug-point] ${page.name} ${mode}[${index}] ${candidate.space} ` +
                  `(${candidate.x.toFixed(1)},${candidate.y.toFixed(1)}) ` +
                  `word=${JSON.stringify(r.word)} via=${r.via} ok=${r.success}`
              )
            }
          }
          const points =
            rectSpace === 'physical'
              ? [
                  { x: raw.x, y: raw.y, space: 'physical' },
                  { x: raw.x * dpr, y: raw.y * dpr, space: 'css-scaled' }
                ]
              : [
                  { x: raw.x * dpr, y: raw.y * dpr, space: 'css-scaled' },
                  { x: raw.x, y: raw.y, space: 'raw-css' }
                ]
          const attempts = []
          let best = null
          let matched = false
          for (const point of points) {
            for (let retry = 0; retry < 2 && !matched; retry += 1) {
              try {
                const range = await probeRange(
                  point.x,
                  point.y,
                  probeTimeout,
                  titleNeedle
                )
                const wordNorm = norm(range.word)
                const q6Match =
                  range.success === true &&
                  wordNorm.length > 0 &&
                  selectedNorm.toLowerCase().includes(wordNorm.toLowerCase())
                const attempt = {
                  pointSpace: point.space,
                  retry,
                  x: point.x,
                  y: point.y,
                  success: range.success,
                  match: q6Match,
                  word: range.word,
                  text: range.text,
                  via: range.via,
                  elementControlType: range.elementControlType,
                  errorCode: range.errorCode ?? null,
                  errorMessage: range.errorMessage ?? null,
                  nativeElapsedMs: range.nativeElapsedMs
                }
                attempts.push(attempt)
                if (!best) best = attempt
                if (q6Match) {
                  best = attempt
                  matched = true
                } else if (retry === 0) {
                  // The first RangeFromPoint after a selection change can
                  // observe a stale accessibility tree. Retry the same point
                  // once after the provider settles.
                  await sleep(150)
                }
              } catch (error) {
                attempts.push({ pointSpace: point.space, retry, error: error.message })
              }
            }
            if (matched) break
          }
          sample.q6 = best
            ? { ...best, attempts }
            : { success: false, match: false, attempts }
        }

        samples.push(sample)
        const status = textMatch && rectOk ? 'OK' : 'FAIL'
        console.log(
          `[d1] ${page.name} ${mode}[${index}] ${status} ` +
            `text=${textMatch ? 'match' : JSON.stringify(textNorm.slice(0, 40))} ` +
            `rects=${usableRects.length}/${rects.length} ` +
            `${probe.nativeElapsedMs.toFixed(1)}ms ` +
            `q6=${sample.q6 ? (sample.q6.match ? 'match' : 'FAIL') : 'skip'}`
        )
      }
    }

    const usable = samples.filter((sample) => sample.textMatch && sample.rectOk)
    const latencies = usable.map((sample) => sample.probe.nativeElapsedMs)
    const q6Samples = samples.filter((sample) => sample.q6)
    const q6Matches = q6Samples.filter((sample) => sample.q6.match)
    const errorCodes = {}
    for (const sample of samples) {
      const code = sample.probe?.errorCode ?? sample.error ?? 'None'
      errorCodes[code] = (errorCodes[code] ?? 0) + 1
    }
    const rectSpaces = {}
    for (const sample of samples) {
      if (sample.rectSpace) rectSpaces[sample.rectSpace] = (rectSpaces[sample.rectSpace] ?? 0) + 1
    }

    return {
      name: page.name,
      label: page.label,
      url: page.url,
      selector,
      windowTitle: titleNeedle,
      dpr,
      rectSpaces,
      eligibleBlocks: count,
      total: samples.length,
      success: usable.length,
      successRate: samples.length ? usable.length / samples.length : 0,
      textOnlySuccess: samples.filter((sample) => sample.textMatch).length,
      rectSuccess: samples.filter((sample) => sample.rectOk).length,
      p50Ms: percentile(latencies, 50),
      p95Ms: percentile(latencies, 95),
      maxMs: latencies.length ? Math.max(...latencies) : null,
      errorCodes,
      q6Total: q6Samples.length,
      q6Success: q6Matches.length,
      q6Rate: q6Samples.length ? q6Matches.length / q6Samples.length : null,
      samples
    }
  }

  for (const page of pages) {
    report.pages.push(await runPage(page))
  }
} catch (error) {
  failures.push(`fatal: ${error.stack ?? error.message}`)
} finally {
  try {
    cdp?.close()
  } catch {
    /* ignore */
  }
  if (!keepOpen && child?.pid) {
    try {
      execFileSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { stdio: 'ignore' })
    } catch {
      /* already gone */
    }
  }
  if (!keepOpen) {
    try {
      rmSync(userDataDir, { recursive: true, force: true })
    } catch {
      /* best effort */
    }
  }
  try {
    staticServer?.close()
  } catch {
    /* ignore */
  }
}

// ---- summary --------------------------------------------------------------

report.clipboardAfter = clipboardSequence()
if (report.clipboardBefore != null && report.clipboardAfter != null) {
  report.clipboardWrites = report.clipboardAfter - report.clipboardBefore
}

const allSamples = report.pages.flatMap((page) => page.samples)
const allUsable = allSamples.filter((sample) => sample.textMatch && sample.rectOk)
const allLatencies = allUsable.map((sample) => sample.probe.nativeElapsedMs)
const allQ6 = allSamples.filter((sample) => sample.q6)
const allQ6Matches = allQ6.filter((sample) => sample.q6.match)
const allRectSpaces = {}
for (const sample of allSamples) {
  if (sample.rectSpace) allRectSpaces[sample.rectSpace] = (allRectSpaces[sample.rectSpace] ?? 0) + 1
}

report.summary = {
  total: allSamples.length,
  success: allUsable.length,
  successRate: allSamples.length ? allUsable.length / allSamples.length : 0,
  textOnlySuccess: allSamples.filter((sample) => sample.textMatch).length,
  rectSuccess: allSamples.filter((sample) => sample.rectOk).length,
  rectSpaces: allRectSpaces,
  p50Ms: percentile(allLatencies, 50),
  p95Ms: percentile(allLatencies, 95),
  maxMs: allLatencies.length ? Math.max(...allLatencies) : null,
  q6Total: allQ6.length,
  q6Success: allQ6Matches.length,
  q6Rate: allQ6.length ? allQ6Matches.length / allQ6.length : null,
  clipboardBefore: report.clipboardBefore,
  clipboardAfter: report.clipboardAfter,
  clipboardWrites: report.clipboardWrites
}
report.failures = failures

const pass =
  report.summary.successRate >= 0.9 &&
  report.summary.p95Ms != null &&
  report.summary.p95Ms <= 50 &&
  report.summary.maxMs != null &&
  report.summary.maxMs <= 50 &&
  (report.summary.q6Rate == null || report.summary.q6Rate >= 0.9) &&
  (report.summary.clipboardWrites == null || report.summary.clipboardWrites === 0)

report.summary.pass = pass

writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8')

console.log('\n===== D-1 summary =====')
console.log(`browser: ${report.browserVersion}`)
for (const page of report.pages) {
  console.log(
    `${page.name.padEnd(8)} success=${page.success}/${page.total} ` +
      `(${(page.successRate * 100).toFixed(1)}%) ` +
      `text=${page.textOnlySuccess}/${page.total} rect=${page.rectSuccess}/${page.total} ` +
      `p50=${page.p50Ms?.toFixed(1) ?? '-'}ms p95=${page.p95Ms?.toFixed(1) ?? '-'}ms ` +
      `q6=${page.q6Success}/${page.q6Total}`
  )
}
console.log(
  `TOTAL    success=${report.summary.success}/${report.summary.total} ` +
    `(${(report.summary.successRate * 100).toFixed(1)}%) ` +
    `p50=${report.summary.p50Ms?.toFixed(1) ?? '-'}ms ` +
    `p95=${report.summary.p95Ms?.toFixed(1) ?? '-'}ms ` +
    `max=${report.summary.maxMs?.toFixed(1) ?? '-'}ms ` +
    `q6=${report.summary.q6Success}/${report.summary.q6Total} ` +
    `clipboardWrites=${report.summary.clipboardWrites ?? 'unknown'}`
)
console.log(`report: ${reportPath}`)

if (failures.length > 0) {
  console.error(`\n[d1] FAIL (${failures.length})`)
  for (const failure of failures) console.error(`  - ${failure}`)
  process.exit(1)
}
if (!pass) {
  console.error('\n[d1] FAIL: acceptance gate not met')
  process.exit(1)
}
console.log('\n[d1] PASS')
