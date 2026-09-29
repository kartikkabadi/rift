export interface Options {
  database?: string
}

export interface AtOptions extends Options {
  at?: string
}

export interface InitOptions extends AtOptions {
  /** Fail instead of falling back to a regular copy when the filesystem cannot clone instantly. */
  cowOnly?: boolean
  /** Linux only: set up a fast virtual disk so copies become instant even on filesystems without copy-on-write. Requires root or sudo. */
  cowImage?: boolean
}

export interface CreateOptions extends Options {
  from?: string
  name?: string
  into?: string
  copyAll?: boolean
  hooks?: boolean
  /** Fail instead of falling back to a regular copy when the filesystem cannot clone instantly. */
  cowOnly?: boolean
}

export interface RemoveOptions extends AtOptions {
  all?: boolean
  hooks?: boolean
}

export interface OfOptions extends Options {
  of?: string
}

export type RiftErrorCode =
  | "io"
  | "database"
  | "walk"
  | "invalid_path"
  | "cow_unavailable"
  | "cow_image_setup"
  | "initialization_required"
  | "workspace_not_initialized"
  | "missing_marker"
  | "unsupported_entry"
  | "unsafe_git"
  | "not_managed"
  | "marker_mismatch"
  | "unknown_marker"
  | "already_exists"
  | "names_exhausted"
  | "missing_rift"
  | "inside_source"
  | "invalid_config"
  | "hook_failed"
  | "no_parent"
  | "invalid_request"
  | "panic"
  | "serialization"

export class RiftError extends Error {
  code: RiftErrorCode
  path?: string
  hook?: "precreate" | "postcreate" | "preremove" | "postremove"
  committed?: boolean
  constructor(input: {
    code: RiftErrorCode
    message: string
    path?: string
    hook?: "precreate" | "postcreate" | "preremove" | "postremove"
    committed?: boolean
  })
}

/** The copy mechanism `create` will use for a workspace path. */
export type Backend = "btrfs" | "reflink" | "apfs" | "refs" | "portable"

export interface Probe {
  path: string
  backend: Backend
  /** The filesystem type name when the platform can report it. */
  filesystem: string | null
}

export function init(options?: InitOptions): null
export function create(options?: CreateOptions): string
export function doctor(options?: OfOptions): Probe
export function remove(options?: RemoveOptions & { all: true }): string[]
export function remove(options?: RemoveOptions): void
export function list(options?: OfOptions): string[]
export function ancestors(options?: OfOptions): string[]
export function gc(options?: Options): string[]

export type DiffKind = "added" | "removed" | "changed"

export interface DiffEntry {
  /** Path relative to the compared roots. */
  path: string
  kind: DiffKind
}

/** The file-level changes that turn `from` into `to`. */
export interface TreeDiff {
  from: string
  to: string
  entries: DiffEntry[]
}

/** The changes inside the workspace relative to the workspace it was copied from. */
export function diff(options?: AtOptions): TreeDiff
/** Apply the workspace's changes back into the workspace it was copied from. */
export function land(options?: AtOptions): TreeDiff
/** Pull the source workspace's latest files into this workspace (the reverse of `land`). */
export function sync(options?: AtOptions): TreeDiff
