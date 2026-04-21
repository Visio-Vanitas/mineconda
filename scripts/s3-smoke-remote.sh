#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <mineconda-bin> <project-root>" >&2
  exit 2
fi

BIN="$1"
PROJECT_ROOT="$2"

SSH_TARGET="${MINECONDA_S3_SSH_TARGET:-}"
REMOTE_PORT="${MINECONDA_S3_REMOTE_PORT:-19000}"
LOCAL_PORT="${MINECONDA_S3_LOCAL_PORT:-39000}"
SOURCE_BUCKET="${MINECONDA_S3_SOURCE_BUCKET:-${MINECONDA_S3_BUCKET:-mineconda-source-smoke}}"
CACHE_BUCKET="${MINECONDA_S3_CACHE_BUCKET:-mineconda-cache-smoke}"
SOURCE_OBJECT_KEY="${MINECONDA_S3_OBJECT_KEY:-packs/dev/iris-s3.jar}"
PRUNE_OBJECT_KEY="${MINECONDA_S3_PRUNE_OBJECT_KEY:-prune-test/old-probe.jar}"
REMOTE_PRIVILEGE_SECRET="${MINECONDA_S3_REMOTE_PRIVILEGE_SECRET:-}"
REMOTE_WORKDIR="${MINECONDA_S3_REMOTE_WORKDIR:-/tmp/mineconda-s3-smoke}"
CONTAINER="${MINECONDA_S3_CONTAINER:-mineconda-s3-smoke}"
MINIO_IMAGE="${MINECONDA_S3_MINIO_IMAGE:-quay.io/minio/minio}"
MC_IMAGE="${MINECONDA_S3_MC_IMAGE:-quay.io/minio/mc}"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10)

require_command() {
  local name="$1"
  command -v "$name" >/dev/null 2>&1 || {
    echo "$name is required for experimental s3 smoke" >&2
    exit 2
  }
}

replace_toml_section() {
  local file="$1"
  local header="$2"
  local body="$3"
  local temp
  temp="$(mktemp)"
  awk -v header="$header" '
    $0 == header {
      skip = 1
      next
    }
    skip && /^\[/ {
      skip = 0
    }
    !skip {
      print
    }
  ' "$file" >"$temp"
  mv "$temp" "$file"
  printf '\n%s\n%s\n' "$header" "$body" >>"$file"
}

random_token() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 16
    return 0
  fi

  python3 - <<'PY'
import secrets
print(secrets.token_hex(16))
PY
}

if [[ -z "$SSH_TARGET" ]]; then
  echo "MINECONDA_S3_SSH_TARGET is required for experimental s3 smoke" >&2
  exit 2
fi

require_command ssh
require_command curl
require_command rg

ACCESS_KEY="${MINECONDA_S3_ACCESS_KEY:-mineconda-$(random_token | cut -c1-16)}"
SECRET_KEY="${MINECONDA_S3_SECRET_KEY:-$(random_token)}"

export MINECONDA_S3_CACHE_ACCESS_KEY="$ACCESS_KEY"
export MINECONDA_S3_CACHE_SECRET_KEY="$SECRET_KEY"

TUNNEL_PID=""

cleanup() {
  if [[ -n "$TUNNEL_PID" ]] && kill -0 "$TUNNEL_PID" >/dev/null 2>&1; then
    kill "$TUNNEL_PID" >/dev/null 2>&1 || true
    wait "$TUNNEL_PID" >/dev/null 2>&1 || true
  fi

  ssh "${SSH_OPTS[@]}" "$SSH_TARGET" \
    CONTAINER="$CONTAINER" \
    REMOTE_WORKDIR="$REMOTE_WORKDIR" \
    REMOTE_PRIVILEGE_SECRET="$REMOTE_PRIVILEGE_SECRET" \
    'bash -s' <<'EOF' >/dev/null 2>&1 || true
set -euo pipefail
docker_cmd() {
  if docker "$@" >/dev/null 2>&1; then
    return 0
  fi
  if sudo -n docker "$@" >/dev/null 2>&1; then
    return 0
  fi
  if [[ -n "${REMOTE_PRIVILEGE_SECRET}" ]] \
    && printf '%s\n' "${REMOTE_PRIVILEGE_SECRET}" | sudo -S -p '' docker "$@" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}
rm_dir() {
  local target="$1"
  if rm -rf "$target" >/dev/null 2>&1; then
    return 0
  fi
  if sudo -n rm -rf "$target" >/dev/null 2>&1; then
    return 0
  fi
  if [[ -n "${REMOTE_PRIVILEGE_SECRET}" ]]; then
    printf '%s\n' "${REMOTE_PRIVILEGE_SECRET}" | sudo -S -p '' rm -rf "$target" >/dev/null 2>&1 || true
  fi
}
if command -v docker >/dev/null 2>&1; then
  docker_cmd rm -f "$CONTAINER" || true
fi
rm_dir "$REMOTE_WORKDIR"
EOF
}
trap cleanup EXIT

echo "[s3-smoke] deploy experimental s3 service on remote target"
ssh "${SSH_OPTS[@]}" "$SSH_TARGET" \
  CONTAINER="$CONTAINER" \
  REMOTE_PORT="$REMOTE_PORT" \
  ACCESS_KEY="$ACCESS_KEY" \
  SECRET_KEY="$SECRET_KEY" \
  REMOTE_PRIVILEGE_SECRET="$REMOTE_PRIVILEGE_SECRET" \
  REMOTE_WORKDIR="$REMOTE_WORKDIR" \
  SOURCE_BUCKET="$SOURCE_BUCKET" \
  CACHE_BUCKET="$CACHE_BUCKET" \
  SOURCE_OBJECT_KEY="$SOURCE_OBJECT_KEY" \
  PRUNE_OBJECT_KEY="$PRUNE_OBJECT_KEY" \
  MINIO_IMAGE="$MINIO_IMAGE" \
  MC_IMAGE="$MC_IMAGE" \
  'bash -s' <<'EOF'
set -euo pipefail
command -v docker >/dev/null
command -v curl >/dev/null

DOCKER_AUTH_MODE="direct"
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_AUTH_MODE="sudo-n"
  elif [[ -n "${REMOTE_PRIVILEGE_SECRET}" ]] \
    && printf '%s\n' "${REMOTE_PRIVILEGE_SECRET}" | sudo -S -p '' docker info >/dev/null 2>&1; then
    DOCKER_AUTH_MODE="sudo-password"
  else
    echo "docker is unavailable for current user and sudo docker access failed" >&2
    exit 1
  fi
fi

docker_cmd() {
  if [[ "$DOCKER_AUTH_MODE" == "sudo-n" ]]; then
    sudo -n docker "$@"
  elif [[ "$DOCKER_AUTH_MODE" == "sudo-password" ]]; then
    printf '%s\n' "${REMOTE_PRIVILEGE_SECRET}" | sudo -S -p '' docker "$@"
  else
    docker "$@"
  fi
}

rm_dir() {
  local target="$1"
  if rm -rf "$target" >/dev/null 2>&1; then
    return 0
  fi

  if [[ "$DOCKER_AUTH_MODE" == "sudo-n" ]]; then
    sudo -n rm -rf "$target"
  elif [[ "$DOCKER_AUTH_MODE" == "sudo-password" ]]; then
    printf '%s\n' "${REMOTE_PRIVILEGE_SECRET}" | sudo -S -p '' rm -rf "$target"
  else
    rm -rf "$target"
  fi
}

docker_cmd rm -f "$CONTAINER" >/dev/null 2>&1 || true
rm_dir "$REMOTE_WORKDIR"
mkdir -p "$REMOTE_WORKDIR/data"
printf 'fake s3 jar from mineconda smoke\n' > "$REMOTE_WORKDIR/iris-s3.jar"
printf 'stale cache object for remote prune smoke\n' > "$REMOTE_WORKDIR/prune-probe.jar"

docker_cmd run -d --name "$CONTAINER" \
  -p "$REMOTE_PORT:9000" \
  -e MINIO_ROOT_USER="$ACCESS_KEY" \
  -e MINIO_ROOT_PASSWORD="$SECRET_KEY" \
  -v "$REMOTE_WORKDIR/data:/data" \
  "$MINIO_IMAGE" server /data >/dev/null

for i in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:${REMOTE_PORT}/minio/health/live" >/dev/null; then
    break
  fi
  sleep 1
done
curl -fsS "http://127.0.0.1:${REMOTE_PORT}/minio/health/live" >/dev/null

MC_HOST="http://${ACCESS_KEY}:${SECRET_KEY}@127.0.0.1:${REMOTE_PORT}"
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  "$MC_IMAGE" mb -p "local/${SOURCE_BUCKET}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  "$MC_IMAGE" mb -p "local/${CACHE_BUCKET}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  "$MC_IMAGE" cp "/work/iris-s3.jar" "local/${SOURCE_BUCKET}/${SOURCE_OBJECT_KEY}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  "$MC_IMAGE" cp "/work/prune-probe.jar" "local/${CACHE_BUCKET}/${PRUNE_OBJECT_KEY}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  "$MC_IMAGE" anonymous set public "local/${SOURCE_BUCKET}" >/dev/null
EOF

echo "[s3-smoke] open local tunnel to experimental s3 service"
ssh "${SSH_OPTS[@]}" -o ExitOnForwardFailure=yes -N \
  -L "${LOCAL_PORT}:127.0.0.1:${REMOTE_PORT}" \
  "$SSH_TARGET" >/dev/null 2>&1 &
TUNNEL_PID=$!
kill -0 "$TUNNEL_PID"

for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${LOCAL_PORT}/minio/health/live" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl -fsS "http://127.0.0.1:${LOCAL_PORT}/minio/health/live" >/dev/null
curl -fsS "http://127.0.0.1:${LOCAL_PORT}/${SOURCE_BUCKET}/${SOURCE_OBJECT_KEY}" >/dev/null

replace_toml_section "$PROJECT_ROOT/mineconda.toml" "[sources.s3]" \
"bucket = \"${SOURCE_BUCKET}\"
public_base_url = \"http://127.0.0.1:${LOCAL_PORT}/${SOURCE_BUCKET}\""

replace_toml_section "$PROJECT_ROOT/mineconda.toml" "[cache.s3]" \
"enabled = true
bucket = \"${CACHE_BUCKET}\"
region = \"us-east-1\"
endpoint = \"http://127.0.0.1:${LOCAL_PORT}\"
prefix = \"cache\"
path_style = true
auth = \"sigv4\"
access_key_env = \"MINECONDA_S3_CACHE_ACCESS_KEY\"
secret_key_env = \"MINECONDA_S3_CACHE_SECRET_KEY\"
upload_enabled = true"

echo "[s3-smoke] add s3 mod and sync"
"$BIN" --root "$PROJECT_ROOT" remove iris-s3 >/dev/null 2>&1 || true
"$BIN" --root "$PROJECT_ROOT" add iris-s3 --source s3 --version "$SOURCE_OBJECT_KEY"
rg -q "source_ref = \"s3://${SOURCE_BUCKET}/${SOURCE_OBJECT_KEY}\"" "$PROJECT_ROOT/mineconda.lock"
source_sync_out="$("$BIN" --root "$PROJECT_ROOT" sync --jobs 2 --verbose-cache)"
printf '%s\n' "$source_sync_out"
printf '%s\n' "$source_sync_out" | rg -q 'sync done: packages='
test -f "$PROJECT_ROOT/mods/$(basename "$SOURCE_OBJECT_KEY")"

echo "[s3-smoke] verify private cache read-through and signed backfill"
mkdir -p "$PROJECT_ROOT/vendor"
printf 's3 cache probe jar\n' > "$PROJECT_ROOT/vendor/s3-cache-probe.jar"
"$BIN" --root "$PROJECT_ROOT" remove s3-cache-probe >/dev/null 2>&1 || true
"$BIN" --root "$PROJECT_ROOT" add s3-cache-probe --source local --version vendor/s3-cache-probe.jar
probe_upload_out="$("$BIN" --root "$PROJECT_ROOT" sync --jobs 2 --verbose-cache)"
printf '%s\n' "$probe_upload_out"
printf '%s\n' "$probe_upload_out" | rg -q 'sync done: packages='

CACHE_NAME="$("$BIN" --root "$PROJECT_ROOT" cache ls | awk '/s3-cache-probe/{print $2; exit}')"
if [[ -z "$CACHE_NAME" ]]; then
  echo "[s3-smoke] failed to locate local cache artifact for s3-cache-probe" >&2
  exit 1
fi
CACHE_DIR="$("$BIN" --root "$PROJECT_ROOT" cache dir)"
rm -f "${CACHE_DIR}/$CACHE_NAME"
rm -f "$PROJECT_ROOT/vendor/s3-cache-probe.jar"
find "$PROJECT_ROOT/mods" -maxdepth 1 -name '*s3-cache-probe*.jar' -delete

probe_restore_out="$("$BIN" --root "$PROJECT_ROOT" sync --locked --jobs 2 --verbose-cache)"
printf '%s\n' "$probe_restore_out"
printf '%s\n' "$probe_restore_out" | rg -q 's3_hits=[1-9]'
test "$(find "$PROJECT_ROOT/mods" -maxdepth 1 -name '*s3-cache-probe*.jar' | wc -l | tr -d ' ')" -ge 1

echo "[s3-smoke] remote prune dry-run and apply"
prune_dry_out="$("$BIN" --root "$PROJECT_ROOT" cache remote-prune --s3 --max-age-days 0 --prefix prune-test --dry-run)"
printf '%s\n' "$prune_dry_out"
printf '%s\n' "$prune_dry_out" | rg -q 'candidates=1'
printf '%s\n' "$prune_dry_out" | rg -q 'deleted=0'

prune_apply_out="$("$BIN" --root "$PROJECT_ROOT" cache remote-prune --s3 --max-age-days 0 --prefix prune-test)"
printf '%s\n' "$prune_apply_out"
printf '%s\n' "$prune_apply_out" | rg -q 'deleted=1'

prune_after_out="$("$BIN" --root "$PROJECT_ROOT" cache remote-prune --s3 --max-age-days 0 --prefix prune-test --dry-run)"
printf '%s\n' "$prune_after_out"
printf '%s\n' "$prune_after_out" | rg -q 'candidates=0'

echo "[s3-smoke] done"
