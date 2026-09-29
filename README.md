# Rift

Instant copy-on-write workspaces for coding agents and parallel work.

> Early software. Behavior, interfaces, and storage details may still change.

## Install

```bash
npm install -g rift-snapshot
# or
bun add -g rift-snapshot
```

Release archives are available from [GitHub Releases](https://github.com/anomalyco/rift/releases/latest).

## Quick Start

```bash
cd ~/code/app
rift init
rift create --name parser-fix
rift list
rift remove ~/code/.rifts/app/parser-fix
rift gc
```

`rift init` registers the project root. `rift create` snapshots the current workspace and prints the new path.
`rift remove` moves a workspace to trash, and `rift gc` deletes trashed workspaces.

Add shell integration to `cd` automatically after `init`, `create`, and `remove`:

```bash
eval "$(rift shell-init zsh)" # or bash or fish
```

```nushell
rift shell-init nushell | save -f (($nu.user-autoload-dirs | first) | path join "rift.nu")
```

```powershell
rift shell-init powershell | Invoke-Expression
```

## Lifecycle Hooks

Add `.rift.toml` to a workspace to run commands around creation and removal:

```toml
version = 1

[[hooks.precreate]]
run = "pnpm run check"

[[hooks.postcreate]]
run = "pnpm install --frozen-lockfile"

[[hooks.postcreate]]
run = "docker compose -p rift-$RIFT_ID up -d"

[[hooks.preremove]]
run = "pnpm run cleanup"

[[hooks.postremove]]
run = "docker compose -p rift-$RIFT_ID down -v"
```

| Hook         | Runs in                     | On failure                                   |
| ------------ | --------------------------- | -------------------------------------------- |
| `precreate`  | source workspace            | nothing is created                           |
| `postcreate` | new workspace               | workspace stays registered; command fails     |
| `preremove`  | selected workspace          | nothing is removed                           |
| `postremove` | trashed or preserved path   | removal stays complete; command fails         |

Hooks receive `RIFT_SOURCE`, `RIFT_DESTINATION`, `RIFT_ID`, and `RIFT_PARENT_ID`. Hook output goes to stderr so
workspace paths on stdout stay machine-readable. Use `--no-hooks` to skip hooks.

## Agents and OpenCode

Each agent can get an isolated copy of your real working state instead of sharing your checkout or starting from a
clean commit.

Use Rift as the worktree backend in OpenCode V2:

```sh
opencode plugin add 'github:anomalyco/rift#v0.0.12::path:plugins/opencode'
```

Or configure it manually:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": ["github:anomalyco/rift#v0.0.12::path:plugins/opencode"]
}
```

After `rift init` in the project root, OpenCode's workspace UI creates Rift workspaces, agents get `rift.create`,
`rift.list`, and `rift.remove` Code Mode tools, and OpenCode's session tools move sessions between workspaces. See
[`plugins/opencode`](plugins/opencode) for options and limitations.

## CLI

### `rift init`

```bash
rift init
rift init --here
rift init --cow-image   # Linux only
```

Selects an existing Rift root above the current directory, or the nearest Git root when no Rift root exists. `--here`
initializes exactly the selected directory. If a registered root lost its `.rift` marker, `init` restores it.

On Linux, `--cow-image` upgrades filesystems without instant copies (ext4, tmpfs, NFS, and so on) to instant copies:
it creates a sparse disk image next to the workspace, formats it btrfs (or xfs when btrfs tools are missing),
loop-mounts it, moves the workspace into it, and links the original path to the moved copy. Requires root or `sudo`
and `mkfs.btrfs` or `mkfs.xfs`. The original directory is kept as `<name>.rift-backup`; delete it once
verified. The mount is recorded in `/etc/fstab` so it survives reboots — remove the line and the `.rifts-images`
directory to undo. The flag is a no-op on filesystems that already clone instantly and errors on other platforms.

### `rift create`

```bash
rift create
rift create --name parser-fix
rift create --into /fast/rifts
rift create --copy-all
rift create --no-hooks
```

Copies the nearest managed workspace, records it as the parent, and prints the new workspace path. Filtered copies
omit regenerable artifacts such as `node_modules`, `target`, virtualenvs, framework caches, `dist`, `build`, and
`coverage`; manifests and lockfiles are kept. `--copy-all` makes an exact copy. `--cow-only` fails instead of falling
back to a regular copy on filesystems that cannot clone instantly.

Git repositories are copied with detached `HEAD`, preserving index and working-tree state. Linked worktrees and
repositories with in-progress merges, rebases, cherry-picks, reverts, bisects, or lock files are rejected.

### `rift list` and `rift ancestors`

```bash
rift list
rift ancestors
```

`list` prints direct child workspaces. `ancestors` prints parent workspaces, nearest first.

### `rift doctor`

```bash
rift doctor
rift doctor --json
```

Reports the filesystem type and which copy method new rifts will use — instant copies (btrfs snapshots, Linux
reflinks, APFS `clonefile`, or ReFS block cloning) or a regular file-by-file copy when the filesystem cannot clone.
Use it to check a machine or a specific path before initializing.

### `rift remove` and `rift gc`

```bash
rift remove                         # trash the current created rift subtree
rift remove -f ~/code/app           # unregister a source root
rift remove --children ~/code/app   # trash descendants, keep the selected workspace
rift remove --no-hooks ~/code/app/task
rift gc                             # delete trash and prune missing entries
```

Removing a created workspace moves its subtree into adjacent `.trash` storage. Unregistering a root requires `-f`,
keeps the source directory, removes its `.rift` marker, and trashes registered descendants.

## How It Works

| Platform             | Instant-copy backend          | Fallback                                                       |
| -------------------- | ------------------------------ | -------------------------------------------------------------- |
| Linux x64 / arm64    | btrfs snapshots, or reflinks   | Regular copy, or `init --cow-image` for a fast virtual disk    |
| macOS arm64 / x64    | APFS `clonefile`               | Regular copy on exFAT, FAT32, network volumes, etc.              |
| Windows x64 / arm64  | ReFS block cloning (Dev Drive) | Regular copy on NTFS and other filesystems                       |

`rift init` works on every filesystem: it picks the instant-copy backend when one exists and otherwise registers the
workspace for regular copies with a note. `--cow-only` requires an instant-copy backend and fails instead. On Linux,
`--cow-image` instead mounts a small btrfs/xfs virtual disk next to the workspace and relocates it there, so even
ext4 hosts get instant copies. Release archives and npm prebuilds cover Linux glibc and musl (any distro, including
Alpine), macOS, and Windows on x64 and arm64.

Each managed workspace has a `.rift` marker containing its ID. A SQLite registry stores paths, parents, and trash
entries. Default storage is adjacent to the source root:

```text
~/code/app/                         source workspace
~/code/.rifts/app/parser-fix/       created workspace
~/code/.rifts/app/.trash/           removed workspace storage
```

Workspaces never overlap, concurrent creates never delete each other's destinations, and removal is a trash operation
until `rift gc` runs.

## JavaScript API

The package selects a Bun or Node binding through conditional exports.

```ts
import { create, doctor, list, remove, gc } from "rift-snapshot";

const workspace = create({ from: process.cwd(), name: "schema-work" });
console.log(doctor({ of: process.cwd() }));
console.log(list({ of: process.cwd() }));
remove({ at: workspace });
gc();
```

```ts
init(options?: { at?: string; cowOnly?: boolean; database?: string }): null
create(options?: { from?: string; name?: string; into?: string; copyAll?: boolean; hooks?: boolean; cowOnly?: boolean; database?: string }): string
doctor(options?: { of?: string; database?: string }): { path: string; backend: string; filesystem: string | null }
remove(options?: { at?: string; all?: false; hooks?: boolean; database?: string }): void
remove(options: { at?: string; all: true; hooks?: boolean; database?: string }): string[]
list(options?: { of?: string; database?: string }): string[]
ancestors(options?: { of?: string; database?: string }): string[]
gc(options?: { database?: string }): string[]
```

On Node.js 26.1 or later the binding uses the experimental FFI API (`node --experimental-ffi`, plus `--allow-ffi`
under the permission model). Older supported Node versions run the same API through the bundled CLI, so no flag is
needed there. `init` initializes exactly `at`; Git-root selection is CLI behavior. Calls are synchronous, so lifecycle
hooks block the caller. Failures throw `RiftError` with `code`, and when relevant `path`, `hook`, and `committed`.

## Development

```bash
cargo test --workspace --locked
./scripts/install.sh
```

`scripts/install.sh` installs an optimized CLI binary to `${CARGO_HOME:-$HOME/.cargo}/bin/rift`.

Benchmark a real `rift create` against a directory, or compare candidate Rift checkouts:

```bash
cargo bench --bench create -- /path/to/linux --samples 10 --output /path/to/results/baseline.json
cargo bench --bench compare -- /path/to/linux --candidate /path/to/rift-a --candidate /path/to/rift-b --samples 10 --output /path/to/results/run-01
```

Results include per-sample timings plus median, minimum, and maximum; `compare` ranks candidates by median.

## License

MIT
