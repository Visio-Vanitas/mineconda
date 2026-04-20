#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-${MINECONDA_BIN:-$ROOT_DIR/target/release/mineconda}}"
TEST_ROOT="${MINECONDA_ACTUAL_SERVER_TEST_ROOT:-$ROOT_DIR/.test/actual-neoforge-server}"
PACK_ROOT="$TEST_ROOT"
MINECONDA_HOME_DIR="$TEST_ROOT/home"
INSTANCE_NAME="${MINECONDA_ACTUAL_SERVER_INSTANCE:-actual-server}"
INSTANCE_DIR="$PACK_ROOT/.mineconda/instances/$INSTANCE_NAME"
NEOFORGE_VERSION="${MINECONDA_NEOFORGE_VERSION:-21.1.211}"
STOP_AFTER_SECONDS="${MINECONDA_ACTUAL_SERVER_STOP_AFTER_SECONDS:-45}"
INSTALLER_URL="https://maven.neoforged.net/releases/net/neoforged/neoforge/${NEOFORGE_VERSION}/neoforge-${NEOFORGE_VERSION}-installer.jar"
SEEDED_RUNTIME_DIR="${MINECONDA_ACTUAL_SERVER_SEEDED_RUNTIME_DIR:-$ROOT_DIR/.test/ci-smoke/home/runtimes/java/temurin/21}"

if [[ ! -x "$BIN" ]]; then
  echo "mineconda binary not found or not executable: $BIN" >&2
  exit 1
fi

rm -rf "$TEST_ROOT"
mkdir -p "$TEST_ROOT"

echo "[actual-server-smoke] init pack"
MINECONDA_HOME="$MINECONDA_HOME_DIR" "$BIN" --root "$TEST_ROOT" init pack --minecraft 1.21.1 --loader neoforge --loader-version "$NEOFORGE_VERSION" >/dev/null

echo "[actual-server-smoke] install managed java"
if [[ -d "$SEEDED_RUNTIME_DIR/payload" ]]; then
  echo "[actual-server-smoke] seed managed java from $SEEDED_RUNTIME_DIR"
  mkdir -p "$MINECONDA_HOME_DIR/runtimes/java/temurin"
  rm -rf "$MINECONDA_HOME_DIR/runtimes/java/temurin/21"
  cp -R "$SEEDED_RUNTIME_DIR" "$MINECONDA_HOME_DIR/runtimes/java/temurin/21"
  rm -f "$MINECONDA_HOME_DIR/runtimes/java/temurin/21/runtime.json"
fi
MINECONDA_HOME="$MINECONDA_HOME_DIR" "$BIN" --root "$PACK_ROOT" env install 21 --use-for-project >/dev/null
JAVA_BIN="$(MINECONDA_HOME="$MINECONDA_HOME_DIR" "$BIN" --root "$PACK_ROOT" env which | sed 's/^.* -> //')"

echo "[actual-server-smoke] prepare project inputs"
mkdir -p "$PACK_ROOT/config"
printf 'eula=true\n' > "$PACK_ROOT/eula.txt"
cat > "$PACK_ROOT/server.properties" <<'PROPS'
motd=mineconda actual server smoke
level-type=minecraft\:flat
max-tick-time=60000
view-distance=6
simulation-distance=4
online-mode=false
PROPS
printf 'actual-server-smoke=true\n' > "$PACK_ROOT/config/actual-server-smoke.toml"

echo "[actual-server-smoke] download official installer"
mkdir -p "$INSTANCE_DIR"
ALL_PROXY= HTTP_PROXY= HTTPS_PROXY= NO_PROXY='*' curl -x '' --fail --location --retry 3 --retry-delay 2 --output "$INSTANCE_DIR/neoforge-${NEOFORGE_VERSION}-installer.jar" "$INSTALLER_URL"

echo "[actual-server-smoke] install official NeoForge server files"
INSTALL_LOG="$TEST_ROOT/installer-output.log"
(
  cd "$INSTANCE_DIR"
  "$JAVA_BIN" -jar "neoforge-${NEOFORGE_VERSION}-installer.jar" --installServer .
) >"$INSTALL_LOG" 2>&1
SERVER_ARGS_FILE="$(find "$INSTANCE_DIR/libraries/net/neoforged/neoforge" -name unix_args.txt | head -n 1)"
if [[ -z "$SERVER_ARGS_FILE" ]]; then
  echo "[actual-server-smoke] installer did not produce unix_args.txt" >&2
  tail -n 160 "$INSTALL_LOG" >&2 || true
  exit 1
fi

echo "[actual-server-smoke] doctor"
MINECONDA_HOME="$MINECONDA_HOME_DIR" "$BIN" --root "$PACK_ROOT" doctor >/dev/null

echo "[actual-server-smoke] run real NeoForge server"
RUN_LOG="$TEST_ROOT/run-output.log"
set +e
{ sleep "$STOP_AFTER_SECONDS"; printf 'stop\n'; } | MINECONDA_HOME="$MINECONDA_HOME_DIR" "$BIN" --root "$PACK_ROOT" run --mode server --instance "$INSTANCE_NAME" --server-jar "${SERVER_ARGS_FILE#$PACK_ROOT/}" --memory 1G >"$RUN_LOG" 2>&1
pipe_status=("${PIPESTATUS[@]}")
status="${pipe_status[1]}"
set -e
if [[ $status -ne 0 ]]; then
  echo "[actual-server-smoke] run failed; tailing captured output" >&2
  tail -n 160 "$RUN_LOG" >&2 || true
  echo "[actual-server-smoke] mineconda run failed with status $status" >&2
  exit $status
fi

LOG_FILE="$INSTANCE_DIR/logs/latest.log"
[[ -f "$INSTANCE_DIR/neoforge-${NEOFORGE_VERSION}-installer.jar" ]]
[[ -f "$INSTANCE_DIR/eula.txt" ]]
[[ -f "$INSTANCE_DIR/server.properties" ]]
[[ -f "$INSTANCE_DIR/config/actual-server-smoke.toml" ]]
[[ -d "$INSTANCE_DIR/libraries" ]]
[[ -f "$LOG_FILE" ]]
rg -q 'Done \(' "$LOG_FILE"
rg -q 'Stopping server' "$LOG_FILE"

echo "[actual-server-smoke] key installer events"
rg -n 'Installing server|The server installed successfully|Installer finished' "$INSTALL_LOG" | sed -n '1,20p' || true
echo "[actual-server-smoke] key run events"
rg -n 'ModLauncher running|Launching target|Done \(' "$RUN_LOG" | sed -n '1,20p' || true
rg -n 'Done \(|Stopping server' "$LOG_FILE" | sed -n '1,20p'

echo "[actual-server-smoke] ok: real NeoForge server launched and stopped cleanly"
