#!/usr/bin/env node

// Builds a per-platform npm package directory:
//   <platform-package>/package.json   — os/cpu-restricted manifest
//   <platform-package>/rift(.exe)     — the CLI binary
//   <platform-package>/<ffi library>  — the FFI shared library
//
// usage: node scripts/prepare-npm-platform-package.mjs <platform> <version> <target-directory> <file>...

import fs from "node:fs"
import path from "node:path"

const [platform, version, target, ...sources] = process.argv.slice(2)
if (!platform || !version || !target || sources.length === 0) {
  console.error(
    "usage: node scripts/prepare-npm-platform-package.mjs <platform> <version> <target-directory> <file>...",
  )
  process.exit(1)
}

const { os, cpu } = {
  "linux-x64": { os: "linux", cpu: "x64" },
  "linux-arm64": { os: "linux", cpu: "arm64" },
  "linux-musl-x64": { os: "linux", cpu: "x64" },
  "linux-musl-arm64": { os: "linux", cpu: "arm64" },
  "darwin-x64": { os: "darwin", cpu: "x64" },
  "darwin-arm64": { os: "darwin", cpu: "arm64" },
  "windows-x64": { os: "win32", cpu: "x64" },
  "windows-arm64": { os: "win32", cpu: "arm64" },
}[platform] ?? {}
if (!os) {
  console.error(`unknown platform: ${platform}`)
  process.exit(1)
}
// musl packages also install on glibc systems; the loader only picks them
// when the main package has no glibc build of its own.

fs.mkdirSync(target, { recursive: true })
for (const source of sources) {
  const destination = path.join(target, path.basename(source))
  fs.copyFileSync(source, destination)
  fs.chmodSync(destination, 0o755)
}

const manifest = {
  name: `rift-snapshot-${platform}`,
  version,
  description: `Rift binaries for ${platform}`,
  license: "MIT",
  repository: {
    type: "git",
    url: "git+https://github.com/anomalyco/rift.git",
  },
  os: [os],
  cpu: [cpu],
  files: [...sources.map((source) => path.basename(source))],
  publishConfig: {
    provenance: true,
  },
}
fs.writeFileSync(path.join(target, "package.json"), `${JSON.stringify(manifest, null, 2)}\n`)
