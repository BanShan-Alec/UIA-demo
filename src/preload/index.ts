import { contextBridge, ipcRenderer, IpcRendererEvent } from 'electron'
import type { AppConfig, ProbeRecord, SetConfigResult } from '../shared/types'

export interface PanelApi {
  onProbeResult(cb: (record: ProbeRecord) => void): () => void
  getState(): Promise<{ latest: ProbeRecord | null; history: ProbeRecord[] }>
  getConfig(): Promise<AppConfig>
  setConfig(patch: Partial<AppConfig>): Promise<SetConfigResult>
  triggerProbe(): Promise<void>
  togglePanel(): void
}

const api: PanelApi = {
  onProbeResult(cb) {
    const listener = (_e: IpcRendererEvent, record: ProbeRecord): void => cb(record)
    ipcRenderer.on('probe:result', listener)
    return () => ipcRenderer.removeListener('probe:result', listener)
  },
  getState: () => ipcRenderer.invoke('probe:getState'),
  getConfig: () => ipcRenderer.invoke('config:get'),
  setConfig: (patch) => ipcRenderer.invoke('config:set', patch),
  triggerProbe: () => ipcRenderer.invoke('probe:trigger'),
  togglePanel: () => ipcRenderer.send('panel:toggle')
}

contextBridge.exposeInMainWorld('api', api)
