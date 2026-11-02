#!/usr/bin/env node
/**
 * ExpandToEnclosingUnit E2E probe.
 *
 * Launches a real Chromium browser (Edge by default) with the DevTools
 * protocol enabled, sets deterministic selections from inside the page, then
 * calls the native `probeExpandUnits` and checks whether every TextUnit
 * returns the expected granularity.
 *
 * The key assertion is Q-1 from the Snapword PRD: a selection inside one
 * paragraph must expand to that paragraph — not to Page/Document.
 *
 * Usage:
 *   node scripts/expand-probe-e2e.mjs
 *   node scripts/expand-probe-e2e.mjs --url https://en.wikipedia.org/wiki/UI_Automation --selector "#mw-content-text p"
 *   node scripts/expand-probe-e2e.mjs --browser chrome --report dist/expand-report.json
 */
import { execFileSync, spawn } from 'node:child_process'
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

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
const isTestPage = !arg('url')
const url = arg('url', pathToFileURL(join(root, 'testpage', 'index.html')).href)
const selector = arg('selector', '#expand-a')
const selector2 = arg('selector2', isTestPage ? '#expand-b' : null)
const needle = arg('needle', isTestPage ? 'brown' : null)
const reportPath = resolve(root, arg('report', 'expand-probe-report.json'))
const keepOpen = flag('keep-open')
const probeTimeout = Number(arg('timeout', '2000'))

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

// ---- in-page selection helper --------------------------------------------

const PAGE_HELPER = String.raw`
(() => {
  const textNodes = (root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
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

  const locate = (root, needle, occurrence = 0) => {
    const nodes = textNodes(root);
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

  const apply = (startNode, startOffset, endNode, endOffset) => {
    const range = document.createRange();
    range.setStart(startNode, startOffset);
    range.setEnd(endNode, endOffset);
    const selection = window.getSelection();
    selection.removeAllRanges();
    selection.addRange(range);
    return selection.toString();
  };

  window.__uiaExpand = {
    select(selector, needle, mode, occurrence = 0) {
      const el = document.querySelector(selector);
      if (!el) return { error: 'selector not found: ' + selector };
      const text = el.textContent || '';
      if (mode === 'element') {
        const nodes = textNodes(el);
        if (nodes.length === 0) return { error: 'element has no text nodes' };
        const selected = apply(nodes[0], 0, nodes[nodes.length - 1], nodes[nodes.length - 1].data.length);
        return { selected, full: text, mode };
      }
      let target = needle;
      if (!target) {
        const pattern =
          mode === 'first-words'
            ? /(?:\S+\s+){0,4}\S+/
            : mode === 'first-ascii-word'
              ? /[A-Za-z][A-Za-z0-9_'-]*/
              : /\S+/;
        const match = text.match(pattern);
        if (!match) return { error: 'no word in element' };
        target = match[0].trim();
      }
      const hit = locate(el, target, occurrence);
      if (!hit) return { error: 'text not found: ' + target };
      const selected = apply(hit.startNode, hit.startOffset, hit.endNode, hit.endOffset);
      return { selected, full: text, needle: target, mode };
    },

    span(selA, needleA, selB, needleB) {
      const a = document.querySelector(selA);
      const b = document.querySelector(selB);
      if (!a || !b) return { error: 'span selector not found' };
      const hitA = locate(a, needleA);
      const hitB = locate(b, needleB);
      if (!hitA || !hitB) return { error: 'span text not found' };
      const selected = apply(hitA.startNode, hitA.startOffset, hitB.endNode, hitB.endOffset);
      return { selected, fullA: a.textContent || '', fullB: b.textContent || '', mode: 'span' };
    }
  };
})();
`

// ---- main -----------------------------------------------------------------

const userDataDir = mkdtempSync(join(tmpdir(), 'uia-expand-'))
let child = null
let cdp = null
const report = {
  generatedAt: new Date().toISOString(),
  browser: browserName,
  browserVersion: null,
  url,
  selector,
  selector2,
  scenarios: [],
  assertions: [],
  raw: null
}

const failures = []
const check = (scenario, name, ok, detail) => {
  const entry = { scenario, name, ok, detail }
  report.assertions.push(entry)
  if (!ok) failures.push(`${scenario} / ${name}: ${detail}`)
}

try {
  console.log(`[expand-probe] launching ${browserName}: ${browserExe}`)
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
      // Edge opens a sync/sign-in WebUI over the page in a fresh profile;
      // guest mode keeps the test page as the active document.
      ...(browserName === 'edge' ? ['--guest', '--enable-features=UiaProvider'] : []),
      // Chromium's renderer accessibility tree is lazy. Without this, the
      // first UIA query only sees the browser chrome (address bar Edit), not
      // the web Document. The Snapword PoC treats full-tree activation as a
      // prerequisite; see README "Chromium 无障碍精简模式".
      '--force-renderer-accessibility',
      url
    ],
    { stdio: 'ignore' }
  )

  const portFile = join(userDataDir, 'DevToolsActivePort')
  await waitFor(() => existsSync(portFile), 20000, 'DevToolsActivePort')
  const [portLine, browserPath] = readFileSync(portFile, 'utf8').trim().split(/\r?\n/)
  const port = Number(portLine)
  if (!port) throw new Error(`invalid DevTools port file: ${portLine}`)

  const version = await fetch(`http://127.0.0.1:${port}/json/version`).then((r) => r.json())
  report.browserVersion = version.Browser
  console.log(`[expand-probe] ${version.Browser}`)

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
  console.log(`[expand-probe] target: ${target.url}`)

  cdp = new Cdp(target.webSocketDebuggerUrl)
  await cdp.open()
  await cdp.send('Page.enable')
  await cdp.send('Runtime.enable')
  // The browser window may not be the OS foreground window (CI / locked
  // desktop). Focus emulation makes the renderer treat the tab as focused,
  // which is a prerequisite for Chromium's native UIA provider to expose the
  // web Document instead of only the browser chrome.
  try {
    await cdp.send('Emulation.setFocusEmulationEnabled', { enabled: true })
  } catch {
    /* older Chromium */
  }

  // Navigate the chosen tab explicitly: a fresh profile may open a welcome
  // page instead of the URL passed on the command line.
  await cdp.send('Page.navigate', { url })

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

  // The page may still be loading; wait for the requested text to appear.
  await waitFor(
    async () => (await evaluate(`!!document.querySelector(${JSON.stringify(selector)})`)) === true,
    30000,
    `selector ${selector}`
  )
  await evaluate(PAGE_HELPER)

  // The probe can target a window by title instead of GetForegroundWindow;
  // a test runner often has no foreground window of its own.
  const pageTitle = await evaluate('document.title')
  const windowTitle = arg('window-title', pageTitle || 'UIA-TEST')
  report.pageTitle = pageTitle
  report.windowTitle = windowTitle
  console.log(`[expand-probe] window title needle: ${JSON.stringify(windowTitle)}`)

  // Scenario table. `needle` is only known up front for the test page; on
  // real pages the helper picks the first word / first five words.
  const scenarios = [
    {
      name: 'word',
      // Real pages may start with CJK text; pick an ASCII word so the Word
      // unit assertion is meaningful for the English-reading use case.
      expression: `__uiaExpand.select(${JSON.stringify(selector)}, ${JSON.stringify(needle)}, ${JSON.stringify(needle ? 'word' : 'first-ascii-word')})`,
      paragraphEqualsElement: true
    },
    {
      name: 'partial',
      expression: `__uiaExpand.select(${JSON.stringify(selector)}, null, 'first-words')`,
      paragraphEqualsElement: true
    },
    {
      name: 'element',
      expression: `__uiaExpand.select(${JSON.stringify(selector)}, null, 'element')`,
      paragraphEqualsElement: true
    }
  ]
  if (selector2) {
    scenarios.push({
      name: 'multi-paragraph',
      expression: isTestPage
        ? `__uiaExpand.span(${JSON.stringify(selector)}, 'brown', ${JSON.stringify(selector2)}, 'quickly')`
        : `__uiaExpand.span(${JSON.stringify(selector)}, null, ${JSON.stringify(selector2)}, null)`,
      paragraphEqualsElement: false
    })
  }
  if (isTestPage) {
    scenarios.push(
      {
        name: 'word-b',
        expression: `__uiaExpand.select('#expand-b', 'vexingly', 'word')`,
        paragraphEqualsElement: true
      },
      {
        name: 'word-nested-inline',
        expression: `__uiaExpand.select('#expand-d', 'boldword', 'word')`,
        paragraphEqualsElement: true
      },
      {
        name: 'multi-paragraph-b-c',
        expression: `__uiaExpand.span('#expand-b', 'vexingly', '#expand-c', 'shorter')`,
        paragraphEqualsElement: false
      }
    )
  }

  // Click once inside the target element so the renderer sees real input
  // focus. The click collapses any selection, so it happens before the
  // scenario loop and every scenario sets its selection afterwards.
  const clickPoint = await evaluate(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return null;
    el.scrollIntoView({ block: 'center' });
    const r = el.getBoundingClientRect();
    return { x: Math.round(r.left + Math.min(12, r.width / 2)), y: Math.round(r.top + Math.min(8, r.height / 2)) };
  })()`)
  if (clickPoint) {
    for (const type of ['mousePressed', 'mouseReleased']) {
      await cdp.send('Input.dispatchMouseEvent', {
        type,
        x: clickPoint.x,
        y: clickPoint.y,
        button: 'left',
        clickCount: 1
      })
    }
  }

  // Warm-up: the first UIA tree enumeration activates Chromium's full
  // accessibility tree. A selection is already needed for GetSelection, so
  // set the first scenario first, then warm up and re-run it.
  const warm = await evaluate(scenarios[0].expression)
  report.scenarios.push({ name: 'warmup', selection: warm })
  await sleep(300)
  await cdp.send('Page.bringToFront')
  activateWindow(child.pid)
  await sleep(300)
  await native.probeExpandUnits(probeTimeout, windowTitle)
  await sleep(600)

  for (const scenario of scenarios) {
    const selection = await evaluate(scenario.expression)
    if (selection?.error) {
      check(scenario.name, 'selection', false, selection.error)
      continue
    }

    await cdp.send('Page.bringToFront')
    activateWindow(child.pid)
    await sleep(250)

    const result = await native.probeExpandUnits(probeTimeout, windowTitle)
    report.scenarios.push({ name: scenario.name, selection, result })

    if (!result.success) {
      check(scenario.name, 'probe success', false, `${result.errorCode}: ${result.errorMessage}`)
      continue
    }

    const unit = (mode, name) => result.results.find((r) => r.mode === mode && r.unit === name)
    const norm = (s) => String(s ?? '').replace(/\r\n/g, '\n').replace(/\s+/g, ' ').trim()
    const expected = norm(selection.full ?? '')
    const selected = norm(selection.selected)
    const paraSelection = unit('selection', 'Paragraph')
    const paraCollapsed = unit('collapsed-start', 'Paragraph')

    check(scenario.name, 'foreground is browser', /msedge|chrome/i.test(result.processName), `processName=${result.processName}`)
    check(scenario.name, 'selection text matches page', norm(result.selectionText) === selected, `uia=${JSON.stringify(norm(result.selectionText).slice(0, 80))} dom=${JSON.stringify(selected.slice(0, 80))}`)

    for (const [mode, para] of [
      ['selection', paraSelection],
      ['collapsed-start', paraCollapsed]
    ]) {
      if (!para) {
        check(scenario.name, `${mode} Paragraph present`, false, 'missing result')
        continue
      }
      if (scenario.paragraphEqualsElement) {
        check(
          scenario.name,
          `${mode} Paragraph == element`,
          norm(para.text) === expected,
          `uiaChars=${para.textChars} elementChars=${expected.length} preview=${JSON.stringify(para.textPreview.slice(0, 100))}`
        )
        check(
          scenario.name,
          `${mode} Paragraph != whole document`,
          !para.equalsDocument && para.textChars < result.documentChars,
          `paragraphChars=${para.textChars} documentChars=${result.documentChars}`
        )
      } else {
        // Chromium clamps a multi-paragraph range to the paragraph that
        // contains the start endpoint — it does not preserve the range and it
        // does not expand to all covered paragraphs. Record that behavior.
        const expectedStart = norm(selection.fullA ?? '')
        check(
          scenario.name,
          `${mode} Paragraph == start paragraph`,
          norm(para.text) === expectedStart,
          `uiaChars=${para.textChars} startParagraphChars=${expectedStart.length} preview=${JSON.stringify(para.textPreview.slice(0, 100))}`
        )
        check(
          scenario.name,
          `${mode} Paragraph != whole document`,
          !para.equalsDocument,
          `paragraphChars=${para.textChars} documentChars=${result.documentChars}`
        )
      }
    }

    // The Document unit is the control: it must cover the whole document.
    const doc = unit('selection', 'Document')
    check(scenario.name, 'Document unit equals document', !!doc?.equalsDocument, `docChars=${doc?.textChars} documentChars=${result.documentChars}`)

    if (scenario.name === 'word') {
      const character = unit('selection', 'Character')
      const word = unit('selection', 'Word')
      check(scenario.name, 'Character unit is one char', character?.textChars === 1, `chars=${character?.textChars}`)
      if (/^[\x00-\x7F]+$/.test(selected)) {
        check(
          scenario.name,
          'Word unit is the selected word',
          norm(word?.text) === selected,
          `uia=${JSON.stringify(norm(word?.text))} dom=${JSON.stringify(selected)}`
        )
      } else {
        // UIA treats each CJK character as a Word unit; record that instead
        // of failing an assertion that assumes whitespace tokenization.
        check(scenario.name, 'Word unit is one CJK character', word?.textChars === 1, `chars=${word?.textChars}`)
      }
    }
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
}

// ---- print + persist ------------------------------------------------------

for (const scenario of report.scenarios) {
  if (!scenario.result?.results) continue
  console.log(`\n=== ${scenario.name} ===`)
  console.log(
    `selection (${scenario.result.selectionChars} chars): ${JSON.stringify(scenario.result.selectionText.slice(0, 100))}`
  )
  console.log(`document: ${scenario.result.documentChars} chars`)
  for (const mode of ['selection', 'collapsed-start']) {
    const rows = scenario.result.results.filter((r) => r.mode === mode)
    if (rows.length === 0) continue
    console.log(`  mode=${mode}`)
    for (const row of rows) {
      console.log(
        `    ${row.unit.padEnd(10)} chars=${String(row.textChars).padStart(5)} doc=${row.equalsDocument ? 'YES' : 'no '} preview=${JSON.stringify(row.textPreview.slice(0, 70))}`
      )
    }
  }
}

report.failures = failures
writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`, 'utf8')
console.log(`\n[expand-probe] report: ${reportPath}`)

if (failures.length > 0) {
  console.error(`\n[expand-probe] FAIL (${failures.length})`)
  for (const failure of failures) console.error(`  - ${failure}`)
  process.exit(1)
}
console.log('\n[expand-probe] PASS')
