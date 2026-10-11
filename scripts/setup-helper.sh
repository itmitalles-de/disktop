#!/usr/bin/env bash
# Build the helper as the regular user, then install it as root with file
# capabilities. Trust boundary: root runs this script from the user's checkout
# and installs the binary the user built, so the administrator who
# authenticates must trust that checkout and build.
set -euo pipefail

destination=/usr/local/libexec/dwtop-collector
receipt=/usr/local/libexec/.dwtop-collector.sha256
capabilities=cap_dac_read_search,cap_sys_ptrace=ep
usage="Usage: $0 [--allow-shared-group] [--allow-owner-change]"
# Upper bounds for enumerating accounts that share the helper group.
account_timeout=10
account_limit=100000
# The helper build is far smaller; a larger source is never copied in full.
copy_limit_mib=256
staged_binary=''
staged_receipt=''

fail() { printf '%s\n' "$*" >&2; exit 1; }
cleanup() {
    for staged in "$staged_binary" "$staged_receipt"; do
        if [[ -n "$staged" && ( -e "$staged" || -L "$staged" ) ]]; then
            rm -- "$staged"
        fi
    done
}
trap cleanup EXIT

safe_directory() {
    local directory="$1" owner mode
    [[ -d "$directory" && ! -L "$directory" ]] || fail "Unsafe helper directory: $directory"
    read -r owner mode < <(stat -c '%u %a' -- "$directory")
    if [[ "$owner" != 0 ]] || (( (8#$mode & 0022) != 0 )); then
        fail "Helper directory must be root-owned and not writable by others: $directory"
    fi
}

check_destination() {
    local owner mode recorded actual
    if [[ -e "$receipt" || -L "$receipt" ]]; then
        [[ -f "$receipt" && ! -L "$receipt" ]] || fail "Unsafe helper receipt: $receipt"
        read -r owner mode < <(stat -c '%u %a' -- "$receipt")
        if [[ "$owner" != 0 ]] || (( (8#$mode & 0022) != 0 )); then
            fail "Unsafe helper receipt permissions: $receipt"
        fi
    fi
    if [[ -e "$destination" || -L "$destination" ]]; then
        [[ -f "$destination" && ! -L "$destination" && -f "$receipt" ]] || fail "Refusing to replace an untracked helper: $destination"
        read -r owner mode < <(stat -c '%u %a' -- "$destination")
        if [[ "$owner" != 0 ]] || (( (8#$mode & 0022) != 0 )); then
            fail "Unsafe existing helper permissions: $destination"
        fi
        recorded="$(cat -- "$receipt")"
        read -r actual _ < <(sha256sum -- "$destination")
        [[ "$recorded" =~ ^[[:xdigit:]]{64}$ && "$actual" == "$recorded" ]] || fail "Refusing to overwrite a changed helper: $destination"
    fi
}

# Print why group $2 may give accounts other than uid $1 access to the helper;
# print nothing only when the group is shown to be private to that account.
# Anything that cannot be determined counts as possibly shared.
shared_group_reasons() {
    local owner_uid="$1" owner_gid="$2" owner_name group_entry group_name members member primary_gid accounts count
    local -a member_list=() others=()
    if ! owner_name="$(id -nu -- "$owner_uid" 2>/dev/null)"; then
        printf -- '- the name of account %s could not be determined\n' "$owner_uid"
        return 0
    fi
    if ! group_entry="$(timeout "$account_timeout" getent group "$owner_gid")"; then
        printf -- '- group %s could not be looked up\n' "$owner_gid"
        return 0
    fi
    IFS=: read -r group_name _ _ members <<<"$group_entry"
    if [[ "$group_name" != "$owner_name" ]]; then
        printf -- '- group %s is not named after %s, so it is not a user-private group\n' "$group_name" "$owner_name"
    fi
    IFS=, read -r -a member_list <<<"$members"
    for member in "${member_list[@]}"; do
        if [[ -n "$member" && "$member" != "$owner_name" ]]; then
            others+=("$member")
        fi
    done
    # Accounts whose primary group it is are not listed as group members.
    if accounts="$(timeout "$account_timeout" getent passwd | head -n "$((account_limit + 1))")" \
        && count="$(wc -l <<<"$accounts")" && (( count <= account_limit )); then
        while IFS=: read -r member _ _ primary_gid _; do
            if [[ "$primary_gid" == "$owner_gid" && "$member" != "$owner_name" ]]; then
                others+=("$member")
            fi
        done <<<"$accounts"
    else
        printf -- '- the accounts with primary group %s could not be listed within %ss and %s entries\n' \
            "$group_name" "$account_timeout" "$account_limit"
    fi
    if (( ${#others[@]} > 0 )); then
        printf -- '- other accounts belong to group %s: %s\n' "$group_name" "$(printf '%s\n' "${others[@]}" | sort -u | paste -sd ' ')"
    fi
}

group_label() {
    local name
    if name="$(timeout "$account_timeout" getent group "$1")"; then
        printf '%s (%s)' "${name%%:*}" "$1"
    else
        printf '%s' "$1"
    fi
}

check_group() {
    local owner_uid="$1" owner_gid="$2" allow="$3" reasons label
    reasons="$(shared_group_reasons "$owner_uid" "$owner_gid")"
    [[ -n "$reasons" ]] || return 0
    label="$(group_label "$owner_gid")"
    if [[ "$allow" == yes ]]; then
        printf 'Warning: helper group %s is or may be shared (--allow-shared-group given):\n%s\n' "$label" "$reasons" >&2
        return 0
    fi
    fail "Refusing to grant helper access to group $label, which is or may be shared:
$reasons
Every member of this group could run the helper and read all processes' I/O and
directory sizes. Use a user-private primary group, or rerun with
--allow-shared-group if every member may see this."
}

check_owner_change() {
    local owner_gid="$1" allow="$2" installed_gid
    [[ -e "$destination" || -L "$destination" ]] || return 0
    installed_gid="$(stat -c '%g' -- "$destination")"
    [[ "$installed_gid" != "$owner_gid" ]] || return 0
    if [[ "$allow" == yes ]]; then
        printf 'Warning: moving helper access from group %s to group %s (--allow-owner-change given).\n' \
            "$(group_label "$installed_gid")" "$(group_label "$owner_gid")" >&2
        return 0
    fi
    fail "Refusing to move helper access: the installed helper belongs to group $(group_label "$installed_gid"),
not $(group_label "$owner_gid"). Its members would lose access. Rerun with
--allow-owner-change to move helper access."
}

if [[ "${1:-}" == --install ]]; then
    (( EUID == 0 && $# >= 5 && $# <= 7 )) || fail 'Internal installation requires root, --install, four parameters and optional flags.'
    # Do not inherit tool lookup or a permissive umask from the invoking user.
    export PATH=/usr/sbin:/usr/bin:/sbin:/bin
    umask 077
    binary="$2"
    owner_uid="$3"
    owner_gid="$4"
    expected_digest="$5"
    allow_shared_group=no
    allow_owner_change=no
    case "$#:${6-}:${7-}" in
        5::) ;;
        6:--allow-shared-group:) allow_shared_group=yes ;;
        6:--allow-owner-change:) allow_owner_change=yes ;;
        7:--allow-shared-group:--allow-owner-change) allow_shared_group=yes; allow_owner_change=yes ;;
        *) fail 'Internal installation accepts only --allow-shared-group and --allow-owner-change, in this order.' ;;
    esac
    [[ "$owner_uid" =~ ^[0-9]+$ && "$owner_uid" != 0 && "$owner_gid" =~ ^[0-9]+$ && "$owner_gid" != 0 ]] || fail 'Helper access must belong to a non-root user and primary group.'
    [[ "$(id -g -- "$owner_uid")" == "$owner_gid" ]] || fail 'Helper group must be the specified primary group.'
    [[ "$expected_digest" =~ ^[[:xdigit:]]{64}$ && -f "$binary" && ! -L "$binary" ]] || fail 'Invalid helper build or digest.'
    # Refuse a symlink anywhere in the path, not only in the last component.
    [[ "$(realpath -e -- "$binary")" == "$binary" ]] || fail 'Helper build path must be absolute and free of symlinks.'
    [[ "$(stat -c '%u' -- "$binary")" == "$owner_uid" ]] || fail 'Helper build must be owned by the requesting user.'
    command -v setcap >/dev/null || fail 'Install libcap2-bin (setcap) before helper setup.'
    command -v getcap >/dev/null || fail 'Install libcap2-bin (getcap) before helper setup.'
    check_group "$owner_uid" "$owner_gid" "$allow_shared_group"

    for directory in / /usr /usr/local; do safe_directory "$directory"; done
    if [[ ! -e /usr/local/libexec && ! -L /usr/local/libexec ]]; then
        install -d -o 0 -g 0 -m 755 /usr/local/libexec
    fi
    safe_directory /usr/local/libexec
    check_destination
    check_owner_change "$owner_gid" "$allow_owner_change"
    staged_binary="$(mktemp /usr/local/libexec/.dwtop-collector.XXXXXX)"
    staged_receipt="$(mktemp /usr/local/libexec/.dwtop-collector-receipt.XXXXXX)"
    # The user can still swap the source (or a parent directory) for a symlink
    # after the checks above. Copy it without following a final symlink into a
    # root:root 0700 file, bounded in size, and grant group access only after
    # the staged copy matches the digest built by the user: content swapped in
    # from a root-only file is never readable by the group and is discarded.
    chmod 700 -- "$staged_binary"
    dd if="$binary" of="$staged_binary" iflag=nofollow,fullblock bs=1M count="$copy_limit_mib" status=none
    read -r installed_digest _ < <(sha256sum -- "$staged_binary")
    [[ "$installed_digest" == "$expected_digest" ]] || fail 'Helper build changed during setup; rerun as your regular user.'
    chgrp -- "+$owner_gid" "$staged_binary"
    chmod 750 -- "$staged_binary"
    printf '%s\n' "$installed_digest" > "$staged_receipt"
    # Changing ownership clears file capabilities, so set them last.
    setcap "$capabilities" "$staged_binary"
    setcap -v "$capabilities" "$staged_binary" >/dev/null
    check_destination
    mv -T -- "$staged_binary" "$destination"
    staged_binary=''
    mv -T -- "$staged_receipt" "$receipt"
    staged_receipt=''
    printf 'Installed root-owned dwtop helper for primary group %s.\n' "$owner_gid"
    getcap "$destination"
    printf 'Start normally: dwtop\n'
    exit 0
fi

allow_shared_group=no
allow_owner_change=no
install_flags=()
for argument in "$@"; do
    case "$argument" in
        --allow-shared-group) allow_shared_group=yes ;;
        --allow-owner-change) allow_owner_change=yes ;;
        *) fail "$usage" ;;
    esac
done
# The root side accepts the flags only in this fixed order.
[[ "$allow_shared_group" == no ]] || install_flags+=(--allow-shared-group)
[[ "$allow_owner_change" == no ]] || install_flags+=(--allow-owner-change)
(( EUID != 0 )) || fail 'Start this script as your regular user; it requests administrator authentication only for installation.'
# Fail early, before building and authenticating; the root side checks again.
check_group "$(id -u)" "$(id -g)" "$allow_shared_group"
check_owner_change "$(id -g)" "$allow_owner_change"
# Resolve symlinks: the root side refuses a build path that contains any.
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cargo build --release --locked --bin dwtop-collector --manifest-path "$repo_dir/Cargo.toml" --target-dir "$repo_dir/target"
binary="$repo_dir/target/release/dwtop-collector"
read -r digest _ < <(sha256sum -- "$binary")
printf 'One-time setup: install %s with /proc and directory read capabilities.\n' "$destination"
printf 'Every member of your primary group (%s) will be able to run it. The terminal UI stays unprivileged.\n' "$(id -gn)"
if command -v pkexec >/dev/null 2>&1; then
    exec pkexec /usr/bin/bash "$repo_dir/scripts/setup-helper.sh" --install "$binary" "$(id -u)" "$(id -g)" "$digest" "${install_flags[@]}"
elif command -v sudo >/dev/null 2>&1; then
    exec sudo /usr/bin/bash "$repo_dir/scripts/setup-helper.sh" --install "$binary" "$(id -u)" "$(id -g)" "$digest" "${install_flags[@]}"
else
    fail 'Administrator authentication is unavailable; install pkexec or sudo to perform the one-time setup.'
fi
