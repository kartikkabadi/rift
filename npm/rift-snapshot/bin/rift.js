#!/usr/bin/env node

import childProcess from "node:child_process"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"

const platform = { darwin: "darwin", linux: "linux", win32: "windows" }[os.platform()]
const arch = { arm64: "arm64", x64: "x64" }[os.arch()]

if (!platform || !arch) {
  console.error(`Unsupported Rift platform: ${os.platform()}-${os.arch()}`)
  process.exit(1)
}

// Static musl builds run on any Linux, so when libc cannot be identified
// musl is the safe guess: a glibc binary would not start on musl at all.
const report = process.report?.getReport?.()
const musl =
  platform === "linux" &&
  (report ? report.header?.glibcVersionRuntime === undefined : fs.existsSync("/etc/alpine-release"))
const flavor = musl ? "linux-musl" : platform

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
const binary = path.join(root, "prebuilds", `${flavor}-${arch}`, platform === "windows" ? "rift.exe" : "rift")
if (!fs.existsSync(binary)) {
  console.error(`Unable to locate the Rift binary for ${flavor}-${arch}. Reinstall rift-snapshot.`)
  process.exit(1)
}

const result = childProcess.spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true,
})
if (result.error) {
  console.error(result.error.message)
  process.exit(1)
}
process.exit(result.status ?? 1)
