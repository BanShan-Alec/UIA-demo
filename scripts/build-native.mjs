// Builds the Rust addon with cargo and copies the DLL to the .node filename
// napi-rs expects (uia_probe.win32-x64-msvc.node). Avoids @napi-rs/cli so the
// build does not depend on a working `node` shim inside cargo build scripts.
import { spawnSync } from 'node:child_process'
import { copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(dirname(fileURLToPath(import.meta.url)))
const nativeDir = join(root, 'native')

const cargo = process.env.CARGO ?? 'cargo'
const res = spawnSync(cargo, ['build', '--release'], {
  cwd: nativeDir,
  stdio: 'inherit',
  shell: process.platform === 'win32'
})
if (res.status !== 0) process.exit(res.status ?? 1)

const src = join(nativeDir, 'target', 'release', 'uia_probe.dll')
const dst = join(nativeDir, 'uia_probe.win32-x64-msvc.node')
if (!existsSync(src)) {
  console.error(`cargo did not produce ${src}`)
  process.exit(1)
}
copyFileSync(src, dst)
mkdirSync(nativeDir, { recursive: true })
console.log(`copied ${src} -> ${dst}`)
