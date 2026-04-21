#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <mineconda-bin> <project-root>" >&2
  exit 2
fi

BIN="$1"
PROJECT_ROOT="$2"

SSH_TARGET="${MINECONDA_S3_SSH_TARGET:-wsl}"
REMOTE_PORT="${MINECONDA_S3_REMOTE_PORT:-19000}"
LOCAL_PORT="${MINECONDA_S3_LOCAL_PORT:-39000}"
SOURCE_BUCKET="${MINECONDA_S3_SOURCE_BUCKET:-${MINECONDA_S3_BUCKET:-mineconda-source-smoke}}"
CACHE_BUCKET="${MINECONDA_S3_CACHE_BUCKET:-mineconda-cache-smoke}"
SOURCE_OBJECT_KEY="${MINECONDA_S3_OBJECT_KEY:-packs/dev/iris-s3.jar}"
PRUNE_OBJECT_KEY="${MINECONDA_S3_PRUNE_OBJECT_KEY:-prune-test/old-probe.jar}"
ACCESS_KEY="${MINECONDA_S3_ACCESS_KEY:-minioadmin}"
SECRET_KEY="${MINECONDA_S3_SECRET_KEY:-minioadmin}"
SUDO_PASSWORD="${MINECONDA_S3_SUDO_PASSWORD:-}"
REMOTE_WORKDIR="${MINECONDA_S3_REMOTE_WORKDIR:-/tmp/mineconda-s3-smoke}"
CONTAINER="${MINECONDA_S3_CONTAINER:-mineconda-s3-smoke}"

export MINECONDA_S3_CACHE_ACCESS_KEY="$ACCESS_KEY"
export MINECONDA_S3_CACHE_SECRET_KEY="$SECRET_KEY"

TUNNEL_PID=""

cleanup() {
  if [[ -n "$TUNNEL_PID" ]] && kill -0 "$TUNNEL_PID" >/dev/null 2>&1; then
    kill "$TUNNEL_PID" >/dev/null 2>&1 || true
    wait "$TUNNEL_PID" >/dev/null 2>&1 || true
  fi

  ssh -o BatchMode=yes "$SSH_TARGET" \
    CONTAINER="$CONTAINER" REMOTE_WORKDIR="$REMOTE_WORKDIR" \
    'bash -s' <<'EOF' >/dev/null 2>&1 || true
set -euo pipefail
if command -v docker >/dev/null 2>&1; then
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || sudo -n docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
fi
rm -rf "$REMOTE_WORKDIR" >/dev/null 2>&1 || true
EOF
}
trap cleanup EXIT

echo "[s3-smoke] deploy minio on ${SSH_TARGET}"
ssh -o BatchMode=yes "$SSH_TARGET" \
  CONTAINER="$CONTAINER" \
  REMOTE_PORT="$REMOTE_PORT" \
  ACCESS_KEY="$ACCESS_KEY" \
  SECRET_KEY="$SECRET_KEY" \
  SUDO_PASSWORD="$SUDO_PASSWORD" \
  REMOTE_WORKDIR="$REMOTE_WORKDIR" \
  SOURCE_BUCKET="$SOURCE_BUCKET" \
  CACHE_BUCKET="$CACHE_BUCKET" \
  SOURCE_OBJECT_KEY="$SOURCE_OBJECT_KEY" \
  PRUNE_OBJECT_KEY="$PRUNE_OBJECT_KEY" \
  'bash -s' <<'EOF'
set -euo pipefail
command -v docker >/dev/null
command -v curl >/dev/null

DOCKER_AUTH_MODE="direct"
if ! docker info >/dev/null 2>&1; then
  if sudo -n docker info >/dev/null 2>&1; then
    DOCKER_AUTH_MODE="sudo-n"
  elif [[ -n "${SUDO_PASSWORD}" ]] \
    && printf '%s\n' "${SUDO_PASSWORD}" | sudo -S -p '' docker info >/dev/null 2>&1; then
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
    printf '%s\n' "${SUDO_PASSWORD}" | sudo -S -p '' docker "$@"
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
    printf '%s\n' "${SUDO_PASSWORD}" | sudo -S -p '' rm -rf "$target"
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
  quay.io/minio/minio server /data >/dev/null

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
  quay.io/minio/mc mb -p "local/${SOURCE_BUCKET}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  quay.io/minio/mc mb -p "local/${CACHE_BUCKET}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  quay.io/minio/mc cp "/work/iris-s3.jar" "local/${SOURCE_BUCKET}/${SOURCE_OBJECT_KEY}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  -v "$REMOTE_WORKDIR:/work" \
  quay.io/minio/mc cp "/work/prune-probe.jar" "local/${CACHE_BUCKET}/${PRUNE_OBJECT_KEY}" >/dev/null
docker_cmd run --rm --network host \
  -e "MC_HOST_local=${MC_HOST}" \
  quay.io/minio/mc anonymous set public "local/${SOURCE_BUCKET}" >/dev/null
EOF

echo "[s3-smoke] open ssh tunnel 127.0.0.1:${LOCAL_PORT} -> ${SSH_TARGET}:127.0.0.1:${REMOTE_PORT}"
ssh -o BatchMode=yes -o ExitOnForwardFailure=yes -N \
  -L "${LOCAL_PORT}:127.0.0.1:${REMOTE_PORT}" \
  "$SSH_TARGET" >/dev/null 2>&1 &
TUNNEL_PID=$!
sleep 1
kill -0 "$TUNNEL_PID"

if ! rg -q '^\[sources\.s3\]' "$PROJECT_ROOT/mineconda.toml"; then
  cat >>"$PROJECT_ROOT/mineconda.toml" <<EOF

[sources.s3]
bucket = "${SOURCE_BUCKET}"
public_base_url = "http://127.0.0.1:${LOCAL_PORT}/${SOURCE_BUCKET}"
EOF
fi

if ! rg -q '^\[cache\.s3\]' "$PROJECT_ROOT/mineconda.toml"; then
  cat >>"$PROJECT_ROOT/mineconda.toml" <<EOF

[cache.s3]
enabled = true
bucket = "${CACHE_BUCKET}"
region = "us-east-1"
endpoint = "http://127.0.0.1:${LOCAL_PORT}"
prefix = "cache"
path_style = true
auth = "sigv4"
access_key_env = "MINECONDA_S3_CACHE_ACCESS_KEY"
secret_key_env = "MINECONDA_S3_CACHE_SECRET_KEY"
upload_enabled = true
EOF
fi

echo "[s3-smoke] add s3 mod and sync"
"$BIN" --root "$PROJECT_ROOT" add iris-s3 --source s3 --version "$SOURCE_OBJECT_KEY"
rg -q "source_ref = \"s3://${SOURCE_BUCKET}/${SOURCE_OBJECT_KEY}\"" "$PROJECT_ROOT/mineconda.lock"
source_sync_out="$("$BIN" --root "$PROJECT_ROOT" sync --jobs 2 --verbose-cache)"
printf '%s\n' "$source_sync_out"
printf '%s\n' "$source_sync_out" | rg -q 'sync done: packages='
test -f "$PROJECT_ROOT/mods/$(basename "$SOURCE_OBJECT_KEY")"

echo "[s3-smoke] verify private cache read-through and signed backfill"
mkdir -p "$PROJECT_ROOT/vendor"
printf 's3 cache probe jar\n' > "$PROJECT_ROOT/vendor/s3-cache-probe.jar"
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
