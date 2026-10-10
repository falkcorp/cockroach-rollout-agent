#!/usr/bin/env bash
# file: scripts/install-rollout-agent.sh
# version: 2.0.0
# guid: 963803cf-366e-40c7-bc9a-67bcd54597a9
# last-edited: 2026-10-10
#
# Installs or updates cockroach-rollout-agent on one CockroachDB host. Stages
# the running CockroachDB version under the agent root and adds a systemd
# drop-in so the CockroachDB unit runs the agent-managed binary from its next
# restart on. /usr/local/bin/cockroach stays a root-owned file the agent never
# touches. Idempotent: safe to rerun. Never starts, stops, or restarts
# CockroachDB, and leaves the agent disabled unless --enable is given.
#
# Root never executes or follows links under the agent root: the cockroach
# user can rewrite anything there. Every command that touches it runs as
# that user.
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
AGENT_LINK=${AGENT_ROOT}/bin/cockroach
DROPIN_NAME=50-cockroach-rollout-agent.conf
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
as_agent() { runuser -u "$AGENT_USER" -- "$@"; }

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

# The operator-facing binary must be a real root-owned file: root runs it.
require_root_owned_system_binary() {
    [[ -f $SYSTEM_BINARY && ! -L $SYSTEM_BINARY ]] ||
        die "$SYSTEM_BINARY must be a regular file; if an older installer made it a symlink, install the official cockroach binary there and rerun"
    [[ $(stat -c %u "$SYSTEM_BINARY") == 0 ]] || die "$SYSTEM_BINARY is not owned by root"
}

# Copies the running version into versions/ and, on first install, points
# bin/cockroach at it. An existing bin/cockroach belongs to the agent and is
# left alone.
stage_current_binary() {
    require_root_owned_system_binary
    local tag version name staged
    tag=$(as_agent "$SYSTEM_BINARY" version --build-tag)
    version=${tag#v}
    [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]*)?$ ]] ||
        die "unexpected build tag from $SYSTEM_BINARY: $tag"
    name="cockroach-v${version}"
    staged="${AGENT_ROOT}/versions/${name}"

    if ! as_agent test -f "$staged"; then
        log "staging ${tag} as ${staged}"
        as_agent install -m 0755 "$SYSTEM_BINARY" "${AGENT_ROOT}/versions/.${name}.partial"
        as_agent mv -f "${AGENT_ROOT}/versions/.${name}.partial" "$staged"
    fi
    [[ $(as_agent "$staged" version --build-tag) == "$tag" ]] ||
        die "staged copy $staged does not report $tag"

    if as_agent test -L "$AGENT_LINK"; then
        log "$AGENT_LINK already exists; leaving it to the agent"
    else
        as_agent ln -sfn "../versions/${name}" "${AGENT_ROOT}/bin/.cockroach.next"
        as_agent mv -Tf "${AGENT_ROOT}/bin/.cockroach.next" "$AGENT_LINK"
        log "$AGENT_LINK -> ../versions/${name}"
    fi
}

# Prints the unit's one effective ExecStart= value, backslash continuations
# kept verbatim, ignoring this script's own drop-in. Fails unless exactly one.
current_exec_start() {
    systemctl cat "$service" | awk -v skip="$dropin_file" '
        /^# \// { file = substr($0, 3); next }
        file == skip { next }
        {
            if (cont) {
                value = value "\n" $0
            } else if ($0 ~ /^[[:space:]]*ExecStart[[:space:]]*=/) {
                sub(/^[[:space:]]*ExecStart[[:space:]]*=[[:space:]]*/, "")
                value = $0
            } else {
                next
            }
            cont = ($0 ~ /\\$/)
            if (!cont) {
                if (value == "") { n = 0 } else { n++; last = value }
            }
        }
        END { if (n != 1) exit 1; print last }'
}

exec_argv() {
    systemctl show --property=ExecStart --value "$service" |
        sed -n 's/.*argv\[\]=\(.*\) ; ignore_errors=.*/\1/p'
}

# Makes the CockroachDB unit run $AGENT_LINK with its existing arguments.
# Takes effect at the unit's next restart, which the agent performs. On a
# rerun the previous drop-in stays in place until the new one is verified.
write_exec_dropin() {
    local exec rest before after previous="" tmp
    # Only safe while the unit itself runs as the agent user: anyone else,
    # root included, would execute a binary the agent user can replace.
    local unit_user
    unit_user=$(systemctl show --property=User --value "$service")
    [[ $unit_user == "$AGENT_USER" ]] ||
        die "$service runs as '${unit_user:-root}', not $AGENT_USER; refusing to point it at an agent-writable binary"
    exec=$(current_exec_start) || die "$service must have exactly one ExecStart"
    rest=${exec#"$SYSTEM_BINARY"}
    [[ $rest != "$exec" && ( -z $rest || $rest == [[:space:]]* ) ]] ||
        die "$service ExecStart does not run $SYSTEM_BINARY: $exec"
    as_agent "$AGENT_LINK" version --build-tag >/dev/null ||
        die "$AGENT_LINK does not run"

    before=$(exec_argv)
    [[ -f $dropin_file ]] && previous=$(cat "$dropin_file")
    install -d -o root -g root -m 0755 "$dropin_dir"
    tmp=$(mktemp "${dropin_file}.XXXXXX")
    printf '# Written by install-rollout-agent.sh. Removed by --uninstall.\n[Service]\nExecStart=\nExecStart=%s%s\n' \
        "$AGENT_LINK" "$rest" >"$tmp"
    chmod 0644 "$tmp"
    mv -f "$tmp" "$dropin_file"
    systemctl daemon-reload

    # Only the executable may change: compare everything after argv[0].
    after=$(exec_argv)
    if [[ $after != "$AGENT_LINK"* || ${after#* } != "${before#* }" ]]; then
        if [[ -n $previous ]]; then
            printf '%s\n' "$previous" >"$dropin_file"
        else
            rm -f "$dropin_file"
        fi
        systemctl daemon-reload
        die "drop-in would change $service arguments; reverted it. before: $before after: $after"
    fi
    log "$service runs $AGENT_LINK from its next restart"
}

# Refuses to drop the override while the agent link runs a different version
# than $SYSTEM_BINARY: the next restart would silently change versions.
remove_exec_dropin() {
    if [[ ! -f $dropin_file ]]; then
        log "no ExecStart drop-in for $service"
        return
    fi
    require_root_owned_system_binary
    local system_tag agent_tag
    system_tag=$(as_agent "$SYSTEM_BINARY" version --build-tag)
    agent_tag=$(as_agent "$AGENT_LINK" version --build-tag) || die "$AGENT_LINK does not run"
    [[ $system_tag == "$agent_tag" ]] ||
        die "$service runs $agent_tag but $SYSTEM_BINARY is $system_tag; install the official $agent_tag binary at $SYSTEM_BINARY first"
    rm -f "$dropin_file"
    rmdir --ignore-fail-on-non-empty "$dropin_dir"
    log "removed $dropin_file; $service runs $SYSTEM_BINARY from its next restart"
}

[[ -n $service ]] || service=$(detect_service)
log "cockroach unit: $service"
dropin_dir="/etc/systemd/system/${service}.d"
dropin_file="${dropin_dir}/${DROPIN_NAME}"

if $uninstall; then
    systemctl disable --now cockroach-rollout-agent.service 2>/dev/null || true
    remove_exec_dropin
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
command -v runuser >/dev/null || die "runuser (util-linux) is not installed"

log "installing agent binary"
install -o root -g root -m 0755 "$agent_binary_src" "${AGENT_BINARY}.next"
mv -f "${AGENT_BINARY}.next" "$AGENT_BINARY"

log "creating directories"
install -d -o "$AGENT_USER" -g "$AGENT_USER" -m 0750 "$AGENT_ROOT" "$LOG_DIR"
as_agent mkdir -p -m 0750 "$AGENT_ROOT/bin" "$AGENT_ROOT/versions" "$AGENT_ROOT/artifacts"
install -d -o root -g root -m 0755 /etc/cockroach-rollout-agent
# Root-owned so root never writes into a directory the agent user controls;
# the agent only reads these.
install -d -o root -g "$AGENT_USER" -m 0750 "$CERTS_DIR"

log "installing client certificate"
install -o root -g "$AGENT_USER" -m 0644 "$certs_src/ca.crt" "$CERTS_DIR/ca.crt"
install -o root -g "$AGENT_USER" -m 0644 "$certs_src/client.rollout.crt" \
    "$CERTS_DIR/client.rollout.crt"
key_tmp=$(mktemp "$CERTS_DIR/.key.XXXXXX")
# `openssl pkey` writes PKCS#8 whether the input is PKCS#1 or PKCS#8.
openssl pkey -in "$certs_src/client.rollout.key" -out "$key_tmp"
chown "root:$AGENT_USER" "$key_tmp"
chmod 0640 "$key_tmp"
mv -f "$key_tmp" "$CERTS_DIR/client.rollout.pk8"

stage_current_binary

log "writing env, unit, and polkit rule"
render "$EXAMPLES_DIR/cockroach-rollout-agent.env.example" "$ENV_FILE" 0640 "root:$AGENT_USER"
render "$EXAMPLES_DIR/cockroach-rollout-agent.service" "$UNIT_FILE" 0644 root:root
install -d -o root -g root -m 0755 "$(dirname "$POLKIT_FILE")"
render "$EXAMPLES_DIR/50-cockroach-rollout-agent.rules" "$POLKIT_FILE" 0644 root:root
systemctl daemon-reload
write_exec_dropin

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
