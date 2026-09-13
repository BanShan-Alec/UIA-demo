import { app } from 'electron'
import { readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import type { AppConfig } from '../shared/types'

const CONFIG_FILE = resolve(app.getAppPath(), 'uia-config.json')

function loadFile(): Partial<AppConfig> {
  try {
    return JSON.parse(readFileSync(CONFIG_FILE, 'utf-8')) as Partial<AppConfig>
  } catch {
    return {}
  }
}

// Precedence: config file > environment variable > default.
export function loadConfig(): AppConfig {
  const file = loadFile()
  return {
    hotkey: file.hotkey ?? process.env.UIA_HOTKEY ?? 'Control+Shift+2',
    panelHotkey: file.panelHotkey ?? 'Control+Shift+1',
    timeoutMs: file.timeoutMs ?? Number(process.env.UIA_TIMEOUT_MS ?? 500),
    alwaysOnTop: file.alwaysOnTop ?? true
  }
}

export function saveConfig(config: AppConfig): void {
  try {
    writeFileSync(CONFIG_FILE, JSON.stringify(config, null, 2) + '\n')
  } catch {
    /* config file is best-effort; runtime config still applies */
  }
}
