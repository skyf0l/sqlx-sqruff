#!/usr/bin/env bash
# Usage: check-unpublished.sh NAME VERSION [NAME VERSION]...
#
# Looks up each crate version in the crates.io sparse index. Exits 0 when none
# of them is on crates.io, 1 after naming each one that is, and any other
# status when the index cannot be read. A yanked version counts as published,
# because crates.io never accepts the same version twice.
set -euo pipefail

if [ $# -eq 0 ] || [ $(($# % 2)) -ne 0 ]; then
  echo "usage: $0 NAME VERSION [NAME VERSION]..." >&2
  exit 2
fi

index_file=$(mktemp)
trap 'rm -f "$index_file"' EXIT

published=0
while [ $# -gt 0 ]; do
  name=$1
  version=$2
  shift 2
  # The index path of a name of 4 or more characters is `ab/cd/abcd...`.
  lower=${name,,}
  if [ ${#lower} -lt 4 ]; then
    echo "error: $name: crate names shorter than 4 characters are not supported" >&2
    exit 2
  fi
  url="https://index.crates.io/${lower:0:2}/${lower:2:2}/${lower}"
  status=$(curl --silent --show-error --location --write-out '%{http_code}' --output "$index_file" "$url") || exit 2
  case $status in
    200)
      # The index file holds one JSON object per published version.
      count=$(jq --slurp --arg version "$version" 'map(select(.vers == $version)) | length' "$index_file")
      if [ "$count" -gt 0 ]; then
        echo "$name $version is already on crates.io"
        published=1
      fi
      ;;
    404)
      # No version of this crate was ever published.
      ;;
    *)
      echo "error: $url: HTTP status $status" >&2
      exit 2
      ;;
  esac
done
exit "$published"
