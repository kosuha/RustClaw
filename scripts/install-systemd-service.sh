#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Install rust-claw systemd service from template.

Usage:
  sudo ./scripts/install-systemd-service.sh [options]

Options:
  --user <name>           Linux user for systemd service. Default: SUDO_USER or USER
  --project-root <path>   Absolute path to rust-claw project. Default: repo root
  --service-name <name>   systemd service name. Default: rust-claw
  --no-start              Do not run "systemctl enable --now"
  --print-only            Print rendered service file to stdout only
  -h, --help              Show this help
EOF
}

error() {
  echo "error: $*" >&2
  exit 1
}

escape_sed_replacement() {
  printf '%s' "$1" | sed 's/[&|]/\\&/g'
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

RUN_USER="${SUDO_USER:-${USER:-}}"
PROJECT_ROOT="${DEFAULT_PROJECT_ROOT}"
SERVICE_NAME="rust-claw"
ENABLE_NOW=1
PRINT_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user)
      [[ $# -ge 2 ]] || error "--user requires a value"
      RUN_USER="$2"
      shift 2
      ;;
    --project-root)
      [[ $# -ge 2 ]] || error "--project-root requires a value"
      PROJECT_ROOT="$2"
      shift 2
      ;;
    --service-name)
      [[ $# -ge 2 ]] || error "--service-name requires a value"
      SERVICE_NAME="$2"
      shift 2
      ;;
    --no-start)
      ENABLE_NOW=0
      shift
      ;;
    --print-only)
      PRINT_ONLY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      error "unknown option: $1"
      ;;
  esac
done

[[ -n "${RUN_USER}" ]] || error "cannot detect service user; pass --user <name>"

PROJECT_ROOT="$(cd "${PROJECT_ROOT}" && pwd)"
TEMPLATE_PATH="${PROJECT_ROOT}/docs/systemd/rust-claw.service"
ENV_FILE="${PROJECT_ROOT}/.env"
BIN_PATH="${PROJECT_ROOT}/target/release/rust_claw"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"

[[ -f "${TEMPLATE_PATH}" ]] || error "template not found: ${TEMPLATE_PATH}"
[[ -f "${ENV_FILE}" ]] || error ".env not found: ${ENV_FILE} (copy from .env.example first)"
[[ -x "${BIN_PATH}" ]] || error "binary not found: ${BIN_PATH} (run 'cargo build --release')"

escaped_user="$(escape_sed_replacement "${RUN_USER}")"
escaped_root="$(escape_sed_replacement "${PROJECT_ROOT}")"

rendered="$(mktemp)"
trap 'rm -f "${rendered}"' EXIT

sed \
  -e "s|YOUR_LINUX_USER|${escaped_user}|g" \
  -e "s|/ABSOLUTE/PATH/TO/rust-claw|${escaped_root}|g" \
  "${TEMPLATE_PATH}" > "${rendered}"

if [[ "${PRINT_ONLY}" -eq 1 ]]; then
  cat "${rendered}"
  exit 0
fi

if [[ "${EUID}" -ne 0 ]]; then
  error "run as root (example: sudo ./scripts/install-systemd-service.sh)"
fi

install -m 0644 "${rendered}" "${SERVICE_PATH}"
systemctl daemon-reload

if [[ "${ENABLE_NOW}" -eq 1 ]]; then
  systemctl enable --now "${SERVICE_NAME}"
  echo "installed and started: ${SERVICE_PATH}"
else
  echo "installed: ${SERVICE_PATH}"
  echo "start manually with: sudo systemctl enable --now ${SERVICE_NAME}"
fi
