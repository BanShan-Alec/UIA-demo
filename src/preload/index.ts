import { contextBridge, ipcRenderer, IpcRendererEvent } from 'electron'
import type { AppConfig, DotSelection, ProbeRecord, SetConfigResult } from '../shared/types'

export interface PanelApi {
  onProbeResult(cb: (record: ProbeRecord) => void): () => void
  onDotSelection(cb: (selection: DotSelection) => void): () => void
  getState(): Promise<{ latest: ProbeRecord | null; history: ProbeRecord[] }>
  getLatestDotSelection(): Promise<DotSelection | null>
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
  onDotSelection(cb) {
    const listener = (_e: IpcRendererEvent, selection: DotSelection): void => cb(selection)
    ipcRenderer.on('dot:selection', listener)
    return () => ipcRenderer.removeListener('dot:selection', listener)
  },
  getState: () => ipcRenderer.invoke('probe:getState'),
  getLatestDotSelection: () => ipcRenderer.invoke('dot:getLatest'),
  getConfig: () => ipcRenderer.invoke('config:get'),
  setConfig: (patch) => ipcRenderer.invoke('config:set', patch),
  triggerProbe: () => ipcRenderer.invoke('probe:trigger'),
  togglePanel: () => ipcRenderer.send('panel:toggle')
}

contextBridge.exposeInMainWorld('api', api)
