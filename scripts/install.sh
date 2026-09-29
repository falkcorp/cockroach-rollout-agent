#!/usr/bin/env bash
# file: scripts/install.sh
# version: 2.0.0
# guid: a57bc5a4-1b0d-491d-9fb5-b17a33a16f52

set -euo pipefail

readonly REPO="${CROACH_ROLLOUT_REPO:-jdfalk/cockroach-rollout-agent}"
readonly PAGES_BASE_URL="${CROACH_ROLLOUT_PAGES_BASE_URL:-https://jdfalk.github.io/cockroach-rollout-agent}"
readonly GITHUB_BASE_URL="${CROACH_ROLLOUT_GITHUB_BASE_URL:-https://github.com}"
readonly BINARY_NAME="cockroach-rollout-agent"

version="latest"
install_systemd="false"
install_dir="${CROACH_ROLLOUT_INSTALL_DIR:-/usr/local/bin}"

usage() {
  cat <<'USAGE'
Install cockroach-rollout-agent for Linux.

Usage:
  install.sh [--version VERSION] [--install-dir DIR] [--with-systemd]

Environment:
  CROACH_ROLLOUT_REPO             GitHub repo, default jdfalk/cockroach-rollout-agent
  CROACH_ROLLOUT_INSTALL_DIR      Install directory, default /usr/local/bin
  CROACH_ROLLOUT_PAGES_BASE_URL   Pages URL for service/env templates

Examples:
  curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash
  curl -fsSL https://jdfalk.github.io/cockroach-rollout-agent/install.sh | sudo bash -s -- --with-systemd
USAGE
}

while (($#)); do
  case "$1" in
    --version)
      version="${2:?--version requires a value}"
      shift 2
      ;;
    --install-dir)
      install_dir="${2:?--install-dir requires a value}"
      shift 2
      ;;
    --with-systemd)
      install_systemd="true"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "required command not found: $1" >&2
    exit 1
  fi
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64)
      echo "amd64"
      ;;
    aarch64 | arm64)
      echo "arm64"
      ;;
    *)
      echo "unsupported architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

asset_base_url() {
  if [[ "${version}" == "latest" ]]; then
    echo "${GITHUB_BASE_URL}/${REPO}/releases/latest/download"
  else
    echo "${GITHUB_BASE_URL}/${REPO}/releases/download/${version}"
  fi
}

download() {
  local url="$1"
  local destination="$2"
  curl --fail --location --show-error --silent --output "${destination}" "${url}"
}

install_binary() {
  local source_binary="$1"
  install -d -m 0755 "${install_dir}"
  install -m 0755 "${source_binary}" "${install_dir}/${BINARY_NAME}"
}

# Installs the per-host setup kit rather than a ready-to-enable unit: the
# unit, env file, and polkit rule all need this host's CockroachDB unit name
# and SQL address, and the binary layout must be converted first.
install_systemd_files() {
  require_command systemctl
  local kit=/usr/local/share/cockroach-rollout-agent
  install -d -m 0755 "${kit}/scripts" "${kit}/examples"
  download "${PAGES_BASE_URL}/install-rollout-agent.sh" "${kit}/scripts/install-rollout-agent.sh"
  chmod 0755 "${kit}/scripts/install-rollout-agent.sh"
  local file
  for file in cockroach-rollout-agent.service cockroach-rollout-agent.env.example 50-cockroach-rollout-agent.rules; do
    download "${PAGES_BASE_URL}/${file}" "${kit}/examples/${file}"
  done
  echo "setup kit installed in ${kit}; finish with:"
  echo "  sudo ${kit}/scripts/install-rollout-agent.sh --agent-binary ${install_dir}/${BINARY_NAME} \\"
  echo "    --sql-addr <this-node-ip>:<sql-port> --certs-src <dir with ca.crt, client.rollout.crt, client.rollout.key>"
}

main() {
  require_command curl
  require_command tar
  require_command sha256sum
  require_command install
  require_command uname

  local arch
  arch="$(detect_arch)"

  local base_url
  base_url="$(asset_base_url)"

  local tmpdir
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "${tmpdir}"' EXIT

  local archive="${BINARY_NAME}-linux-${arch}.tar.gz"
  download "${base_url}/${archive}" "${tmpdir}/${archive}"
  download "${base_url}/SHA256SUMS" "${tmpdir}/SHA256SUMS"

  (
    cd "${tmpdir}"
    grep "  ${archive}$" SHA256SUMS | sha256sum --check --status
    tar -xzf "${archive}"
  )

  install_binary "${tmpdir}/${BINARY_NAME}"
  echo "installed ${install_dir}/${BINARY_NAME}"

  if [[ "${install_systemd}" == "true" ]]; then
    install_systemd_files
  fi
}

main "$@"
