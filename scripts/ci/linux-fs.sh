#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: linux-fs.sh <btrfs|xfs-reflink|xfs-no-reflink|ext4|tmpfs|zfs|f2fs> [-- <command> ...]" >&2
}

if [[ $# -lt 1 ]]; then
  usage
  exit 2
fi

filesystem="$1"
shift

if [[ "${1:-}" == "--" ]]; then
  shift
fi

workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
runner_temp="${RUNNER_TEMP:-$(mktemp -d)}"
mountpoint="/mnt/rift-${filesystem}"
checkout="${mountpoint}/rift"
image="${runner_temp}/rift-${filesystem}.img"
pool=""
loop_device=""

section() {
  printf '\n==> %s\n' "$*"
}

fail() {
  echo "::error::linux filesystem fixture failed: $*" >&2
  exit 1
}

expected_fstype() {
  case "${filesystem}" in
    btrfs)
      echo "btrfs"
      ;;
    xfs-reflink | xfs-no-reflink)
      echo "xfs"
      ;;
    ext4)
      echo "ext4"
      ;;
    tmpfs)
      echo "tmpfs"
      ;;
    f2fs)
      echo "f2fs"
      ;;
    zfs)
      echo "zfs"
      ;;
    *)
      echo "unsupported filesystem fixture: ${filesystem}" >&2
      exit 2
      ;;
  esac
}

print_mount_state() {
  section "$1"
  findmnt -T "${mountpoint}" -o TARGET,SOURCE,FSTYPE,OPTIONS || true
  df -T "${mountpoint}" || true
}

cleanup() {
  local status=$?
  set +e

  if [[ ${status} -ne 0 ]]; then
    print_mount_state "mount state before cleanup"
  fi

  if [[ -n "${pool}" ]]; then
    sudo zpool destroy "${pool}" >/dev/null 2>&1
  elif mountpoint -q "${mountpoint}"; then
    sudo umount "${mountpoint}" >/dev/null 2>&1
  fi

  if [[ -n "${loop_device}" ]]; then
    sudo losetup --detach "${loop_device}" >/dev/null 2>&1
  fi

  sudo rmdir "${mountpoint}" >/dev/null 2>&1
  exit "${status}"
}

trap cleanup EXIT

print_tool_version() {
  local label="$1"
  shift

  if command -v "$1" >/dev/null 2>&1; then
    "$@" || true
  else
    echo "${label}: unavailable"
  fi
}

print_tool_versions() {
  section "filesystem tool versions"
  print_tool_version "btrfs" btrfs --version
  print_tool_version "mkfs.xfs" mkfs.xfs -V
  print_tool_version "zfs" zfs version
  print_tool_version "mount" mount --version
}

format_loopback() {
  local size="$1"
  shift

  truncate -s "${size}" "${image}"
  "$@" "${image}"
}

mount_loopback() {
  loop_device="$(sudo losetup --find --show "${image}")"
  sudo mount "${loop_device}" "${mountpoint}"
}

create_mount() {
  section "mount setup for ${filesystem}"
  sudo mkdir -p "${mountpoint}"

  case "${filesystem}" in
    btrfs)
      format_loopback 1G mkfs.btrfs -f
      mount_loopback
      ;;
    xfs-reflink)
      format_loopback 1G mkfs.xfs -f -m reflink=1
      mount_loopback
      ;;
    xfs-no-reflink)
      format_loopback 1G mkfs.xfs -f -m reflink=0
      mount_loopback
      ;;
    ext4)
      format_loopback 1G mkfs.ext4 -F
      mount_loopback
      ;;
    tmpfs)
      sudo mount -t tmpfs -o size=1G tmpfs "${mountpoint}"
      ;;
    f2fs)
      format_loopback 1G mkfs.f2fs -f
      sudo modprobe f2fs || true
      mount_loopback
      ;;
    zfs)
      pool="rift-ci-${GITHUB_RUN_ID:-$$}-${RANDOM}"
      truncate -s 1G "${image}"
      sudo modprobe zfs || true
      sudo zpool create -f -m "${mountpoint}" "${pool}" "${image}"
      ;;
    *)
      echo "unsupported filesystem fixture: ${filesystem}" >&2
      exit 2
      ;;
  esac
}

copy_checkout() {
  section "copy checkout"
  sudo chown "${USER}:${USER}" "${mountpoint}"
  mkdir -p "${checkout}"
  cp -a "${workspace}/." "${checkout}"
}

print_diagnostics() {
  section "fixture diagnostics"
  uname -a
  findmnt -T "${checkout}" -o TARGET,SOURCE,FSTYPE,OPTIONS
  df -T "${checkout}"
  stat -f -c "%T" "${checkout}"
  print_tool_versions
}

current_fstype() {
  findmnt -T "$1" -n -o FSTYPE
}

assert_expected_fstype() {
  local actual expected
  expected="$(expected_fstype)"
  actual="$(current_fstype "${checkout}")"

  [[ "${actual}" == "${expected}" ]] || fail "${checkout} is ${actual}, expected ${expected}"
}

assert_writable_checkout() {
  local probe="${checkout}/.rift-fixture-write-probe"

  touch "${probe}" || fail "${checkout} is not writable by ${USER}"
  rm -f "${probe}"
}

assert_same_device() {
  local left="$1"
  local right="$2"
  local left_device right_device

  left_device="$(stat -c "%d" "${left}")"
  right_device="$(stat -c "%d" "${right}")"

  [[ "${left_device}" == "${right_device}" ]] ||
    fail "${right} is not on the same device as ${left}"
}

assert_registry_temp_path() {
  local temp registry

  temp="$(mktemp -d "${checkout}/.rift-registry-preflight.XXXXXX")"
  registry="${temp}/registry.sqlite"
  : >"${registry}"

  assert_same_device "${checkout}" "${registry}"
  [[ "$(current_fstype "${registry}")" == "$(expected_fstype)" ]] ||
    fail "registry preflight path is not on $(expected_fstype)"

  rm -rf "${temp}"
}

reflink_probe() {
  local temp source clone output status

  temp="$(mktemp -d "${checkout}/.rift-reflink-preflight.XXXXXX")"
  source="${temp}/source"
  clone="${temp}/clone"
  printf 'rift native reflink preflight\n' >"${source}"

  if output="$(cp --reflink=always "${source}" "${clone}" 2>&1)"; then
    rm -rf "${temp}"
    return 0
  else
    status=$?
    rm -rf "${temp}"
    echo "${output}"
    return "${status}"
  fi
}

assert_reflink_probe_passes() {
  local output

  if ! output="$(reflink_probe 2>&1)"; then
    fail "${filesystem} should support native reflinks, but the probe failed: ${output}"
  fi

  echo "native reflink probe passed"
}

assert_reflink_probe_fails() {
  local output

  if output="$(reflink_probe 2>&1)"; then
    fail "${filesystem} unexpectedly supports the native reflink probe"
  fi

  echo "native reflink probe failed as expected: ${output}"
}

assert_btrfs_subvolume_probe() {
  local probe="${checkout}/.rift-btrfs-subvolume-preflight"
  local output

  if ! output="$(btrfs subvolume create "${probe}" 2>&1)"; then
    fail "btrfs subvolume create probe failed: ${output}"
  fi

  if ! output="$(btrfs subvolume delete "${probe}" 2>&1)"; then
    case "${output}" in
      *"Operation not permitted"* | *"Permission denied"*)
        echo "btrfs subvolume delete probe cleanup needs fallback: ${output}"
        rm -rf "${probe}"
        ;;
      *)
        fail "btrfs subvolume delete probe failed: ${output}"
        ;;
    esac
  fi

  [[ ! -e "${probe}" ]] || fail "btrfs subvolume probe cleanup left ${probe}"
  echo "btrfs subvolume probe passed"
}

assert_capability_probe() {
  section "capability preflight"

  case "${filesystem}" in
    btrfs)
      assert_btrfs_subvolume_probe
      ;;
    xfs-reflink | zfs)
      assert_reflink_probe_passes
      ;;
    xfs-no-reflink | ext4 | tmpfs | f2fs)
      assert_reflink_probe_fails
      ;;
  esac
}

assert_fixture() {
  section "fixture preflight"
  [[ -d "${checkout}" ]] || fail "mounted checkout does not exist: ${checkout}"
  assert_writable_checkout
  assert_expected_fstype
  assert_registry_temp_path
  assert_capability_probe
}

run_command() {
  if [[ $# -eq 0 ]]; then
    section "no command supplied"
    return 0
  fi

  section "rust tests"
  (
    cd "${checkout}"
    "$@"
  )
}

main() {
  print_tool_versions
  create_mount
  copy_checkout
  print_diagnostics
  assert_fixture
  run_command "$@"
}

main "$@"
