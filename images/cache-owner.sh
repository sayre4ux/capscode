#!/bin/sh
# Keep a persistent cache volume writable by the unprivileged capsule user.
ensure_cache_owner() {
  _cache_owner_path=$1
  _cache_owner_user=$2
  _cache_owner_uid=$3
  _cache_owner_gid=$4

  if [ -e "$_cache_owner_path" ] || mountpoint -q "$_cache_owner_path"; then
    _cache_owner_current=$(stat -c '%u:%g' "$_cache_owner_path")
    if [ "$_cache_owner_current" != "$_cache_owner_uid:$_cache_owner_gid" ]; then
      chown "$_cache_owner_user:$_cache_owner_user" "$_cache_owner_path"
    fi
  fi
}
