#!/usr/bin/env bash

set -euo pipefail

readonly REPOSITORY="${REPOSITORY:-tiejunhu/ones-mcp-cli}"
readonly BINARY_NAME="omc"
readonly API_BASE="https://api.github.com/repos/${REPOSITORY}"

log() {
  printf '%s\n' "$*"
}

fatal() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || fatal "missing required command: $1"
}

fetch_text() {
  local url="$1"
  local response

  if command -v curl >/dev/null 2>&1; then
    if response="$(
      curl -fsSL \
        -H "Accept: application/vnd.github+json" \
        -H "User-Agent: ${BINARY_NAME}-install-script" \
        "$url" 2>&1
    )"; then
      printf '%s' "$response"
      return
    fi

    fatal "failed to fetch ${url}: ${response}"
  fi

  if command -v wget >/dev/null 2>&1; then
    if response="$(
      wget -qO- \
        --header="Accept: application/vnd.github+json" \
        --header="User-Agent: ${BINARY_NAME}-install-script" \
        "$url" 2>&1
    )"; then
      printf '%s' "$response"
      return
    fi

    fatal "failed to fetch ${url}: ${response}"
  fi

  fatal "install requires curl or wget"
}

download_file() {
  local url="$1"
  local output="$2"
  local error_output

  if command -v curl >/dev/null 2>&1; then
    if error_output="$(curl -fsSL --retry 3 --output "$output" "$url" 2>&1)"; then
      return
    fi

    fatal "failed to download ${url}: ${error_output}"
  fi

  if command -v wget >/dev/null 2>&1; then
    if error_output="$(wget -O "$output" "$url" 2>&1)"; then
      return
    fi

    fatal "failed to download ${url}: ${error_output}"
  fi

  fatal "install requires curl or wget"
}

normalize_version() {
  local version="$1"
  if [[ -z "$version" ]]; then
    return
  fi

  if [[ "$version" == v* ]]; then
    printf '%s\n' "$version"
    return
  fi

  printf 'v%s\n' "$version"
}

release_url() {
  local version="${1:-}"
  if [[ -n "$version" ]]; then
    printf '%s/releases/tags/%s\n' "$API_BASE" "$version"
    return
  fi

  printf '%s/releases/latest\n' "$API_BASE"
}

detect_target() {
  local os
  local arch

  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)
      os="unknown-linux-gnu"
      ;;
    Darwin)
      os="apple-darwin"
      ;;
    *)
      fatal "unsupported operating system: $os"
      ;;
  esac

  case "$arch" in
    x86_64 | amd64)
      arch="x86_64"
      ;;
    arm64 | aarch64)
      arch="aarch64"
      ;;
    *)
      fatal "unsupported architecture: $arch"
      ;;
  esac

  printf '%s-%s\n' "$arch" "$os"
}

compact_json() {
  tr -d '\n'
}

json_value() {
  local json="$1"
  local key="$2"
  local regex

  regex="\"${key}\":\"([^\"]+)\""
  if [[ "$json" =~ $regex ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  fi
}

asset_value() {
  local json="$1"
  local asset_name="$2"
  local key="$3"
  local suffix
  local regex

  suffix="${json#*\"name\":\"${asset_name}\"}"
  if [[ "$suffix" == "$json" ]]; then
    return
  fi

  regex="\"${key}\":\"([^\"]+)\""
  if [[ "$suffix" =~ $regex ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  fi
}

sha256_file() {
  local file="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
    return
  fi

  return 1
}

default_install_dir() {
  if [[ -n "${INSTALL_DIR:-}" ]]; then
    printf '%s\n' "${INSTALL_DIR}"
    return
  fi

  if [[ "$(id -u)" -eq 0 ]]; then
    if [[ -d "/opt/homebrew/bin" ]]; then
      printf '/opt/homebrew/bin\n'
      return
    fi

    printf '/usr/local/bin\n'
    return
  fi

  if [[ -d "/opt/homebrew/bin" && -w "/opt/homebrew/bin" ]]; then
    printf '/opt/homebrew/bin\n'
    return
  fi

  if [[ -d "/usr/local/bin" && -w "/usr/local/bin" ]]; then
    printf '/usr/local/bin\n'
    return
  fi

  [[ -n "${HOME:-}" ]] || fatal "HOME is not set; specify INSTALL_DIR explicitly"
  printf '%s/.local/bin\n' "${HOME}"
}

install_binary() {
  local source="$1"
  local destination="$2"

  if command -v install >/dev/null 2>&1; then
    install -m 0755 "$source" "$destination"
    return
  fi

  cp "$source" "$destination"
  chmod 0755 "$destination"
}

path_contains_dir() {
  local dir="$1"
  local entry

  IFS=':' read -r -a path_entries <<< "${PATH:-}"
  for entry in "${path_entries[@]}"; do
    if [[ "$entry" == "$dir" ]]; then
      return 0
    fi
  done

  return 1
}

main() {
  need_cmd tar
  need_cmd mktemp
  need_cmd uname

  local version="${VERSION:-}"
  version="$(normalize_version "$version")"

  local target
  target="$(detect_target)"

  local install_dir
  install_dir="$(default_install_dir)"
  mkdir -p "$install_dir"

  if [[ ! -w "$install_dir" ]]; then
    fatal "install directory is not writable: $install_dir; rerun with sudo or set INSTALL_DIR"
  fi

  local release_json
  release_json="$(fetch_text "$(release_url "$version")")"
  release_json="$(printf '%s' "$release_json" | compact_json)"

  local release_tag
  release_tag="$(json_value "$release_json" "tag_name")"
  [[ -n "$release_tag" ]] || fatal "failed to resolve release tag"

  local asset_name
  asset_name="${BINARY_NAME}-${release_tag}-${target}.tar.gz"

  local download_url
  download_url="$(asset_value "$release_json" "$asset_name" "browser_download_url")"
  [[ -n "$download_url" ]] || fatal "no release asset found for target ${target}"

  local expected_digest
  expected_digest="$(asset_value "$release_json" "$asset_name" "digest")"
  expected_digest="${expected_digest#sha256:}"

  temp_dir="$(mktemp -d)"
  trap 'rm -rf -- "$temp_dir"' EXIT

  local archive_path
  archive_path="${temp_dir}/${asset_name}"

  log "Downloading ${asset_name}"
  download_file "$download_url" "$archive_path"

  if [[ -n "$expected_digest" ]]; then
    local actual_digest
    if actual_digest="$(sha256_file "$archive_path")"; then
      [[ "$actual_digest" == "$expected_digest" ]] || fatal "checksum mismatch for ${asset_name}"
    else
      log "Skipping checksum verification because no SHA-256 tool is available"
    fi
  fi

  tar -xzf "$archive_path" -C "$temp_dir"

  local extracted_binary
  extracted_binary="${temp_dir}/${BINARY_NAME}"
  [[ -f "$extracted_binary" ]] || fatal "archive did not contain ${BINARY_NAME}"

  install_binary "$extracted_binary" "${install_dir}/${BINARY_NAME}"

  log "Installed ${BINARY_NAME} ${release_tag} to ${install_dir}/${BINARY_NAME}"
  if ! path_contains_dir "$install_dir"; then
    log "Warning: ${install_dir} is not in PATH"
    log "Add it to your shell profile before running ${BINARY_NAME} without the full path"
  fi
}

main "$@"
