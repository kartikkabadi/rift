import { CString, dlopen, ptr } from "bun:ffi"
import childProcess from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"
import { createRequire } from "node:module"

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]
if (!platform || !arch) throw new Error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)

const report = process.report?.getReport?.()
const musl =
  platform === "linux" &&
  (report ? report.header?.glibcVersionRuntime === undefined : fs.existsSync("/etc/alpine-release"))

const flavor = musl ? "linux-musl" : platform

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
// Prefer the installed per-platform package; fall back to the bundled
// prebuilds shipped inside the launcher package itself.
const require = createRequire(import.meta.url)
let directory = path.join(root, "prebuilds", `${flavor}-${arch}`)
try {
  directory = path.dirname(require.resolve(`rift-snapshot-${flavor}-${arch}/package.json`))
} catch {}
const libraryPath = path.join(
  directory,
  platform === "windows" ? "rift_ffi.dll" : platform === "darwin" ? "librift_ffi.dylib" : "librift_ffi.so",
)
const binaryPath = path.join(directory, platform === "windows" ? "rift.exe" : "rift")

const encoder = new TextEncoder()
function loadFfi(libraryPath) {
  const { symbols } = dlopen(libraryPath, {
    rift_ffi_call: { args: ["ptr"], returns: "ptr" },
    rift_ffi_free: { args: ["ptr"], returns: "void" },
  })
  return (request) => {
    const input = encoder.encode(`${JSON.stringify(request)}\0`)
    const output = symbols.rift_ffi_call(ptr(input))
    if (!output) throw new Error("Rift native library returned no response")
    let response
    try {
      response = JSON.parse(new CString(output).toString())
    } finally {
      symbols.rift_ffi_free(output)
    }
    return response
  }
}

// Platforms without a shared library (musl ships the CLI only) use the
// `rift rpc` subprocess entry point instead.
function loadSubprocess(binaryPath) {
  return (request) => {
    const result = childProcess.spawnSync(binaryPath, ["rpc"], {
      input: JSON.stringify(request),
      encoding: "utf8",
      windowsHide: true,
    })
    if (result.error) throw result.error
    if (result.status !== 0) {
      throw new Error(`Rift failed (exit ${result.status}): ${result.stderr || result.stdout}`.trim())
    }
    return JSON.parse(result.stdout)
  }
}

let ffi = null
if (fs.existsSync(libraryPath)) {
  try {
    ffi = loadFfi(libraryPath)
  } catch {}
}
const run = ffi ?? (() => {
  if (!fs.existsSync(binaryPath)) {
    throw new Error(`Unable to locate the Rift binaries for ${flavor}-${arch}. Reinstall rift-snapshot.`)
  }
  return loadSubprocess(binaryPath)
})()

function call(request) {
  const response = run(request)
  if (response.status === "error") throw new RiftError(response.error)
  return response.value
}

export class RiftError extends Error {
  constructor({ code, message, path, hook, committed }) {
    super(message)
    this.name = "RiftError"
    this.code = code
    this.path = path
    this.hook = hook
    this.committed = committed
  }
}

export function init({ at = process.cwd(), cowOnly, cowImage, database } = {}) {
  return call({ command: "init", at, cowOnly, cowImage, database })
}

export function create({ from = process.cwd(), name, into, copyAll, hooks, cowOnly, database } = {}) {
  return call({ command: "create", from, name, into, copyAll, hooks, cowOnly, database })
}

export function remove({ at = process.cwd(), all = false, hooks, database } = {}) {
  const result = call({ command: "remove", at, all, hooks, database })
  return all ? result : undefined
}

export function list({ of = process.cwd(), database } = {}) {
  return call({ command: "list", of, database })
}

export function ancestors({ of = process.cwd(), database } = {}) {
  return call({ command: "ancestors", of, database })
}

export function doctor({ of = process.cwd(), database } = {}) {
  return call({ command: "doctor", of, database })
}


export function diff({ at = process.cwd(), database } = {}) {
  return call({ command: "diff", at, database })
}

export function land({ at = process.cwd(), onConflict, filesOnly, database } = {}) {
  return call({ command: "land", at, onConflict, filesOnly, database })
}

export function sync({ at = process.cwd(), onConflict, filesOnly, database } = {}) {
  return call({ command: "sync", at, onConflict, filesOnly, database })
}

export function gc({ database } = {}) {
  return call({ command: "gc", database })
}
