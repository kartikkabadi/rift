import childProcess from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]
if (!platform || !arch) throw new Error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)

const report = process.report?.getReport?.()
const musl = platform === "linux" && report?.header?.glibcVersionRuntime === undefined
const flavor = musl ? "linux-musl" : platform

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
const directory = path.join(root, "prebuilds", `${flavor}-${arch}`)
const libraryPath = path.join(
  directory,
  platform === "windows" ? "rift_ffi.dll" : platform === "darwin" ? "librift_ffi.dylib" : "librift_ffi.so",
)
const binaryPath = path.join(directory, platform === "windows" ? "rift.exe" : "rift")

// node:ffi is experimental and only exists in very recent Node versions.
// Every other platform uses the `rift rpc` subprocess entry point instead.
async function loadFfi(libraryPath) {
  const { dlopen, toString } = await import("node:ffi")
  const { functions } = dlopen(libraryPath, {
    rift_ffi_call: { parameters: ["string"], result: "pointer" },
    rift_ffi_free: { parameters: ["pointer"], result: "void" },
  })
  return (request) => {
    const output = functions.rift_ffi_call(JSON.stringify(request))
    if (!output) throw new Error("Rift native library returned no response")
    let response
    try {
      response = JSON.parse(toString(output))
    } finally {
      functions.rift_ffi_free(output)
    }
    return response
  }
}

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

let call
if (fs.existsSync(libraryPath)) {
  try {
    const ffi = await loadFfi(libraryPath)
    call = (request) => respond(ffi(request))
  } catch {
    call = null
  }
}
if (!call) {
  if (!fs.existsSync(binaryPath)) {
    throw new Error(`Unable to locate the Rift binaries for ${flavor}-${arch}. Reinstall rift-snapshot.`)
  }
  const subprocess = loadSubprocess(binaryPath)
  call = (request) => respond(subprocess(request))
}

function respond(response) {
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

export function gc({ database } = {}) {
  return call({ command: "gc", database })
}
