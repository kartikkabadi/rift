#!/usr/bin/env bash
# Exercises `rift init --cow-image` end to end on a filesystem that cannot
# copy-on-write. Run on a Linux host with passwordless sudo and a filesystem
# tool installed (btrfs-progs or xfsprogs).
set -euo pipefail

rift="${1:?usage: linux-cow-image.sh <path-to-rift-binary>}"

fail() {
  echo "::error::cow-image e2e failed: $*" >&2
  exit 1
}

work="$(mktemp -d)"
export HOME="${work}/home"
mkdir -p "${HOME}"
project="${work}/app"
mkdir -p "${project}/nested"
echo "hello" > "${project}/nested/file.txt"
git -C "${project}" init -q
git -C "${project}" add -A
git -C "${project}" -c user.email=ci@example.com -c user.name=ci commit -qm init

section() { printf '\n==> %s\n' "$*"; }

section "preflight: host filesystem cannot clone"
host_fstype="$(findmnt -T "${project}" -n -o FSTYPE)"
echo "host filesystem: ${host_fstype}"
"${rift}" doctor "${project}"
case "${host_fstype}" in
  btrfs | xfs | zfs | bcachefs)
    fail "host filesystem already supports instant copies; nothing to prove"
    ;;
esac

section "rift init --cow-image"
"${rift}" init --cow-image "${project}"

section "post-init state"
[[ -L "${project}" ]] || fail "${project} is not a symlink after cow-image setup"
real="$(readlink -f "${project}")"
echo "workspace resolves to ${real}"
[[ "$(findmnt -T "${real}" -n -o FSTYPE)" =~ ^(btrfs|xfs)$ ]] ||
  fail "mounted workspace is not on a copy-on-write filesystem: $(findmnt -T "${real}" -n -o FSTYPE)"
[[ -d "${work}/app.rift-backup" ]] || fail "backup directory missing"
[[ "$(cat "${real}/nested/file.txt")" == "hello" ]] || fail "workspace content lost"

section "rift doctor reports instant copies"
"${rift}" doctor "${project}" | tee "${work}/doctor.txt"
grep -qi "instant" "${work}/doctor.txt" || fail "doctor did not report instant copies"

section "rift create produces a clone inside the image"
clone="$("${rift}" create "${project}" --name clone1)"
[[ "${clone}" == "${real%/*}"/* ]] || fail "clone ${clone} is not inside the image"
[[ "$(cat "${clone}/nested/file.txt")" == "hello" ]] || fail "clone content differs"

section "rift list / remove"
"${rift}" list "${project}" | grep -q "${clone}" || fail "clone missing from list"
"${rift}" remove "${clone}"
[[ ! -e "${clone}" ]] || fail "clone still exists after remove"

section "cleanup"
mountpoint="$(findmnt -T "${real}" -n -o TARGET)"
sudo umount "${mountpoint}" || fail "could not unmount ${mountpoint}"
rm -rf "${work}"

echo "cow-image e2e passed"
