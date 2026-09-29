#!/usr/bin/env bash
# file: scripts/install-rollout-agent.sh
# version: 1.0.0
# guid: 963803cf-366e-40c7-bc9a-67bcd54597a9
# last-edited: 2026-09-29
#
# Installs or updates cockroach-rollout-agent on one CockroachDB host, and
# converts /usr/local/bin/cockroach to the agent-owned symlink layout.
# Idempotent: safe to rerun. Never starts, stops, or restarts CockroachDB,
# and leaves the agent disabled unless --enable is given.
#
# Usage (as root, from a directory holding this script's repo layout):
#   install-rollout-agent.sh --agent-binary PATH --sql-addr HOST:PORT \
#       --certs-src DIR [--service UNIT] [--enable]
#   install-rollout-agent.sh --uninstall [--service UNIT]
#
# --certs-src must contain ca.crt, client.rollout.crt, and client.rollout.key
# (PKCS#1 or PKCS#8; it is converted to PKCS#8).

set -euo pipefail

AGENT_USER=cockroach
AGENT_ROOT=/var/lib/cockroach-rollout-agent
LOG_DIR=/var/log/cockroach-rollout-agent
CERTS_DIR=/etc/cockroach-rollout-agent/certs
ENV_FILE=/etc/cockroach-rollout-agent.env
UNIT_FILE=/etc/systemd/system/cockroach-rollout-agent.service
POLKIT_FILE=/etc/polkit-1/rules.d/50-cockroach-rollout-agent.rules
SYSTEM_BINARY=/usr/local/bin/cockroach
AGENT_BINARY=/usr/local/bin/cockroach-rollout-agent

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
EXAMPLES_DIR="${SCRIPT_DIR}/../examples"

agent_binary_src=""
sql_addr=""
certs_src=""
service=""
enable=false
uninstall=false

die() {
    echo "error: $*" >&2
    exit 1
}
log() { echo "==> $*"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
    --agent-binary) agent_binary_src=$2; shift 2 ;;
    --sql-addr) sql_addr=$2; shift 2 ;;
    --certs-src) certs_src=$2; shift 2 ;;
    --service) service=$2; shift 2 ;;
    --enable) enable=true; shift ;;
    --uninstall) uninstall=true; shift ;;
    -h | --help) sed -n '2,20p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run as root"
id "$AGENT_USER" >/dev/null 2>&1 || die "user $AGENT_USER does not exist"

detect_service() {
    local candidates=() unit
    for unit in cockroach.service cockroachdb.service; do
        if systemctl cat "$unit" >/dev/null 2>&1; then
            candidates+=("$unit")
        fi
    done
    case ${#candidates[@]} in
    1) echo "${candidates[0]}" ;;
    0) die "no cockroach.service or cockroachdb.service found; pass --service" ;;
    *) die "both cockroach.service and cockroachdb.service exist; pass --service" ;;
    esac
}

render() {
    # render TEMPLATE DEST MODE OWNER
    local template=$1 dest=$2 mode=$3 owner=$4 tmp
    [[ -f $template ]] || die "missing template $template"
    tmp=$(mktemp "${dest}.XXXXXX")
    sed -e "s|@CROACH_ROLLOUT_SERVICE@|${service}|g" \
        -e "s|@NODE_SQL_ADDR@|${sql_addr}|g" \
        "$template" >"$tmp"
    chmod "$mode" "$tmp"
    chown "$owner" "$tmp"
    mv -f "$tmp" "$dest"
}

# Points $SYSTEM_BINARY at the agent layout without ever leaving the path
# missing: the new link is created beside it and renamed over it.
convert_binary_layout() {
    local link="${AGENT_ROOT}/bin/cockroach"
    if [[ -L $SYSTEM_BINARY && $(readlink "$SYSTEM_BINARY") == "$link" ]]; then
        log "binary layout already converted"
        return
    fi
    [[ -f $SYSTEM_BINARY && ! -L $SYSTEM_BINARY ]] ||
        die "$SYSTEM_BINARY is neither a regular file nor our symlink; fix by hand"

    local tag version staged
    tag=$("$SYSTEM_BINARY" version --build-tag)
    version=${tag#v}
    staged="${AGENT_ROOT}/versions/cockroach-v${version}"
    log "staging current binary ${tag} as ${staged}"
    install -o "$AGENT_USER" -g "$AGENT_USER" -m 0755 "$SYSTEM_BINARY" "${staged}.partial"
    mv -f "${staged}.partial" "$staged"
    [[ $("$staged" version --build-tag) == "$tag" ]] || die "staged copy does not report $tag"

    ln -sfn "../versions/cockroach-v${version}" "${AGENT_ROOT}/bin/.cockroach.next"
    chown -h "$AGENT_USER:$AGENT_USER" "${AGENT_ROOT}/bin/.cockroach.next"
    mv -Tf "${AGENT_ROOT}/bin/.cockroach.next" "$link"

    # Keep the original until the rename succeeds, then replace it atomically.
    ln -sfn "$link" "${SYSTEM_BINARY}.rollout-next"
    mv -Tf "${SYSTEM_BINARY}.rollout-next" "$SYSTEM_BINARY"
    [[ $("$SYSTEM_BINARY" version --build-tag) == "$tag" ]] ||
        die "$SYSTEM_BINARY no longer reports $tag after conversion"
    log "$SYSTEM_BINARY -> $link -> cockroach-v${version}"
}

# Restores $SYSTEM_BINARY as a real root-owned file holding whatever version
# is currently active.
revert_binary_layout() {
    if [[ ! -L $SYSTEM_BINARY ]]; then
        log "$SYSTEM_BINARY is already a regular file"
        return
    fi
    local resolved
    resolved=$(readlink -f "$SYSTEM_BINARY")
    [[ -f $resolved ]] || die "$SYSTEM_BINARY resolves to missing $resolved"
    install -o root -g root -m 0755 "$resolved" "${SYSTEM_BINARY}.rollout-restore"
    mv -Tf "${SYSTEM_BINARY}.rollout-restore" "$SYSTEM_BINARY"
    log "$SYSTEM_BINARY restored as a regular file from $resolved"
}

[[ -n $service ]] || service=$(detect_service)
log "cockroach unit: $service"

if $uninstall; then
    systemctl disable --now cockroach-rollout-agent.service 2>/dev/null || true
    revert_binary_layout
    rm -f "$POLKIT_FILE" "$UNIT_FILE"
    systemctl daemon-reload
    log "uninstalled; $AGENT_ROOT, $LOG_DIR, $CERTS_DIR and $ENV_FILE were kept"
    exit 0
fi

[[ -n $agent_binary_src ]] || die "--agent-binary is required"
[[ -n $sql_addr ]] || die "--sql-addr is required (this node's SQL host:port)"
[[ -n $certs_src ]] || die "--certs-src is required"
for file in ca.crt client.rollout.crt client.rollout.key; do
    [[ -f $certs_src/$file ]] || die "missing $certs_src/$file"
done
command -v pkcheck >/dev/null || die "polkit (pkcheck) is not installed"

log "installing agent binary"
install -o root -g root -m 0755 "$agent_binary_src" "${AGENT_BINARY}.next"
mv -f "${AGENT_BINARY}.next" "$AGENT_BINARY"

log "creating directories"
install -d -o "$AGENT_USER" -g "$AGENT_USER" -m 0750 \
    "$AGENT_ROOT" "$AGENT_ROOT/bin" "$AGENT_ROOT/versions" "$AGENT_ROOT/artifacts" "$LOG_DIR"
install -d -o root -g root -m 0755 /etc/cockroach-rollout-agent
install -d -o "$AGENT_USER" -g "$AGENT_USER" -m 0700 "$CERTS_DIR"

log "installing client certificate"
install -o "$AGENT_USER" -g "$AGENT_USER" -m 0644 "$certs_src/ca.crt" "$CERTS_DIR/ca.crt"
install -o "$AGENT_USER" -g "$AGENT_USER" -m 0644 "$certs_src/client.rollout.crt" \
    "$CERTS_DIR/client.rollout.crt"
key_tmp=$(mktemp "$CERTS_DIR/.key.XXXXXX")
# `openssl pkey` writes PKCS#8 whether the input is PKCS#1 or PKCS#8.
openssl pkey -in "$certs_src/client.rollout.key" -out "$key_tmp"
chown "$AGENT_USER:$AGENT_USER" "$key_tmp"
chmod 0600 "$key_tmp"
mv -f "$key_tmp" "$CERTS_DIR/client.rollout.pk8"

convert_binary_layout

log "writing env, unit, and polkit rule"
render "$EXAMPLES_DIR/cockroach-rollout-agent.env.example" "$ENV_FILE" 0640 "root:$AGENT_USER"
render "$EXAMPLES_DIR/cockroach-rollout-agent.service" "$UNIT_FILE" 0644 root:root
install -d -o root -g root -m 0755 "$(dirname "$POLKIT_FILE")"
render "$EXAMPLES_DIR/50-cockroach-rollout-agent.rules" "$POLKIT_FILE" 0644 root:root
systemctl daemon-reload

# Run inside the same sandbox as the real unit, so the writability and
# restart-authorization probes see exactly what the daemon will see.
log "running self-check as $AGENT_USER inside the unit's sandbox"
if ! systemd-run --quiet --wait --pipe --collect \
    --uid="$AGENT_USER" --gid="$AGENT_USER" \
    -p EnvironmentFile="$ENV_FILE" \
    -p NoNewPrivileges=true -p PrivateTmp=true \
    -p ProtectSystem=strict -p ProtectHome=true \
    -p ReadWritePaths="$LOG_DIR $AGENT_ROOT" \
    "$AGENT_BINARY" self-check; then
    die "self-check failed; the agent was NOT enabled"
fi

if $enable; then
    systemctl enable --now cockroach-rollout-agent.service
    log "agent enabled and started"
else
    systemctl disable cockroach-rollout-agent.service 2>/dev/null || true
    log "agent installed but left disabled (pass --enable to start it)"
fi
