#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${MINECONDA_BIN:-$ROOT_DIR/target/debug/mineconda}"

if [[ ! -x "$BIN" ]]; then
  echo "[ci-smoke] mineconda binary not found at $BIN, building..."
  cargo build -p mineconda-cli >/dev/null
fi

WORK_ROOT="$ROOT_DIR/.test/ci-smoke"
PROJECT_ROOT="$WORK_ROOT/mypack"
RUNTIME_SEED_DIR="${MINECONDA_RUNTIME_SEED_DIR:-$ROOT_DIR/.test/runtime-seed/temurin-21}"
export MINECONDA_CACHE_DIR="$WORK_ROOT/cache"
export MINECONDA_HOME="$WORK_ROOT/home"
export MINECONDA_NO_SPINNER=1
export MINECONDA_LANG="${MINECONDA_LANG:-en}"

rm -rf "$WORK_ROOT"
mkdir -p "$PROJECT_ROOT"

echo "[ci-smoke] init"
"$BIN" --root "$PROJECT_ROOT" init mypack --minecraft 1.21.1 --loader neoforge --loader-version latest

echo "[ci-smoke] search iris (default source)"
"$BIN" --root "$PROJECT_ROOT" search iris --limit 5 --page 2 >/dev/null
"$BIN" --root "$PROJECT_ROOT" search iris --limit 5 --page 2 >/dev/null
test -d "$MINECONDA_CACHE_DIR/search-results"
test "$(find "$MINECONDA_CACHE_DIR/search-results" -name '*.json' | wc -l | tr -d ' ')" -ge 1

echo "[ci-smoke] search install-first (default source)"
"$BIN" --root "$PROJECT_ROOT" search embeddium --limit 1 --page 1 --install-first --non-interactive
rg -q 'id = "embeddium"' "$PROJECT_ROOT/mineconda.toml"
test "$(find "$PROJECT_ROOT/mods" -maxdepth 1 -name 'embeddium*.jar' | wc -l | tr -d ' ')" -ge 1

echo "[ci-smoke] search interactive install via pty (Enter -> versions -> install)"
command -v python3 >/dev/null
env -u CI BIN="$BIN" PROJECT_ROOT="$PROJECT_ROOT" python3 <<'PY'
import os
import pty
import select
import signal
import sys
import time

bin_path = os.environ["BIN"]
project_root = os.environ["PROJECT_ROOT"]
argv = [bin_path, "--root", project_root, "search", "ferritecore", "--limit", "1", "--page", "1"]

pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, argv)

deadline = time.time() + 60
captured = []
status = None
next_enter_at = time.time() + 2.0
enter_sent = 0

while time.time() < deadline:
    readable, _, _ = select.select([fd], [], [], 0.2)
    if readable:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        text = data.decode("utf-8", errors="ignore")
        captured.append(text)

    now = time.time()
    if enter_sent < 5 and now >= next_enter_at:
        try:
            os.write(fd, b"\r")
            enter_sent += 1
            next_enter_at = now + 0.8
        except OSError:
            pass

    done, wait_status = os.waitpid(pid, os.WNOHANG)
    if done == pid:
        status = wait_status
        break

if status is None:
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass
    _, status = os.waitpid(pid, 0)

if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
    sys.stderr.write("[ci-smoke] failed: interactive search install exited with error\n")
    sys.stderr.write("".join(captured)[-4000:])
    sys.exit(1)
PY
rg -q 'id = "ferrite-core"' "$PROJECT_ROOT/mineconda.toml"
test "$(find "$PROJECT_ROOT/mods" -maxdepth 1 -iname '*ferrite*core*.jar' | wc -l | tr -d ' ')" -ge 1

echo "[ci-smoke] search interactive quick install via pty (L key)"
env -u CI BIN="$BIN" PROJECT_ROOT="$PROJECT_ROOT" python3 <<'PY'
import os
import pty
import select
import signal
import sys
import time

bin_path = os.environ["BIN"]
project_root = os.environ["PROJECT_ROOT"]
argv = [bin_path, "--root", project_root, "search", "ferritecore", "--limit", "1", "--page", "1"]

pid, fd = pty.fork()
if pid == 0:
    os.execv(bin_path, argv)

deadline = time.time() + 60
captured = []
status = None
l_sent = False
send_l_at = time.time() + 2.0

while time.time() < deadline:
    readable, _, _ = select.select([fd], [], [], 0.2)
    if readable:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        text = data.decode("utf-8", errors="ignore")
        captured.append(text)

    now = time.time()
    if not l_sent and now >= send_l_at:
        try:
            os.write(fd, b"l")
            l_sent = True
        except OSError:
            pass

    done, wait_status = os.waitpid(pid, os.WNOHANG)
    if done == pid:
        status = wait_status
        break

if status is None:
    try:
        os.kill(pid, signal.SIGTERM)
    except OSError:
        pass
    _, status = os.waitpid(pid, 0)

if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
    sys.stderr.write("[ci-smoke] failed: interactive quick install exited with error\n")
    sys.stderr.write("".join(captured)[-4000:])
    sys.exit(1)

joined = "".join(captured).lower()
if "installed" not in joined:
    sys.stderr.write("[ci-smoke] failed: interactive quick install did not trigger install output\n")
    sys.stderr.write(joined[-4000:])
    sys.exit(1)
PY

echo "[ci-smoke] add local JEI fixture"
mkdir -p "$PROJECT_ROOT/vendor"
printf 'fake jei jar\n' > "$PROJECT_ROOT/vendor/jei.jar"
"$BIN" --root "$PROJECT_ROOT" add jei --source local --version vendor/jei.jar

echo "[ci-smoke] ls --status --info"
"$BIN" --root "$PROJECT_ROOT" ls --status --info >/dev/null

echo "[ci-smoke] pin jei from lock"
"$BIN" --root "$PROJECT_ROOT" pin jei --source local --no-lock

echo "[ci-smoke] update jei constraint"
"$BIN" --root "$PROJECT_ROOT" update jei --source local --to vendor/jei.jar

echo "[ci-smoke] upgrade lock (alias)"
"$BIN" --root "$PROJECT_ROOT" upgrade

echo "[ci-smoke] sync --jobs 2 --verbose-cache"
sync_out="$("$BIN" --root "$PROJECT_ROOT" sync --jobs 2 --verbose-cache)"
printf '%s\n' "$sync_out"
printf '%s\n' "$sync_out" | rg -q 'sync done: packages='

echo "[ci-smoke] sync --locked"
"$BIN" --root "$PROJECT_ROOT" sync --locked

echo "[ci-smoke] sync --json"
sync_json="$("$BIN" --root "$PROJECT_ROOT" sync --check --json)"
printf '%s\n' "$sync_json"
JSON_PAYLOAD="$sync_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "sync", payload
assert payload["summary"]["mode"] == "check", payload
assert payload["summary"]["exit_code"] == 0, payload
PY

echo "[ci-smoke] lock diff/status clean"
lock_diff_clean="$("$BIN" --root "$PROJECT_ROOT" lock diff)"
printf '%s\n' "$lock_diff_clean"
printf '%s\n' "$lock_diff_clean" | rg -q 'lock diff: no changes'
status_clean="$("$BIN" --root "$PROJECT_ROOT" status)"
printf '%s\n' "$status_clean"
printf '%s\n' "$status_clean" | rg -q 'status summary: clean'

echo "[ci-smoke] lock diff/status drift detection"
printf 'fake smoke probe\n' > "$PROJECT_ROOT/vendor/smoke-probe.jar"
"$BIN" --root "$PROJECT_ROOT" add smoke-probe --source local --version vendor/smoke-probe.jar --no-lock
set +e
lock_diff_drift="$("$BIN" --root "$PROJECT_ROOT" lock diff 2>&1)"
lock_diff_code=$?
status_drift="$("$BIN" --root "$PROJECT_ROOT" status 2>&1)"
status_drift_code=$?
set -e
test "$lock_diff_code" -eq 2
test "$status_drift_code" -eq 2
printf '%s\n' "$lock_diff_drift"
printf '%s\n' "$lock_diff_drift" | rg -q 'ADD smoke-probe \[local\]'
printf '%s\n' "$status_drift"
printf '%s\n' "$status_drift" | rg -q 'status summary: drift detected'
"$BIN" --root "$PROJECT_ROOT" lock
"$BIN" --root "$PROJECT_ROOT" sync --jobs 2 --verbose-cache

echo "[ci-smoke] cache stats/verify"
cache_stats_json="$("$BIN" --root "$PROJECT_ROOT" cache stats --json)"
printf '%s\n' "$cache_stats_json"
printf '%s\n' "$cache_stats_json" | rg -q '"referenced_files"'
"$BIN" --root "$PROJECT_ROOT" cache verify

echo "[ci-smoke] offline sync from warmed cache"
rm -f "$PROJECT_ROOT/vendor/jei.jar"
find "$PROJECT_ROOT/mods" -maxdepth 1 -type f -name '*.jar' -delete
offline_out="$("$BIN" --root "$PROJECT_ROOT" sync --offline --jobs 2 --verbose-cache)"
printf '%s\n' "$offline_out"
printf '%s\n' "$offline_out" | rg -q 'local_hits='
printf '%s\n' "$offline_out" | rg -q 'origin_downloads=0'
test -f "$PROJECT_ROOT/mods/jei.jar"
test "$(find "$PROJECT_ROOT/mods" -maxdepth 1 -name '*.jar' | wc -l | tr -d ' ')" -ge 3

echo "[ci-smoke] cache dir/ls/clean"
"$BIN" --root "$PROJECT_ROOT" cache dir >/dev/null
"$BIN" --root "$PROJECT_ROOT" cache ls >/dev/null
"$BIN" --root "$PROJECT_ROOT" cache clean

test -f "$PROJECT_ROOT/mineconda.toml"
test -f "$PROJECT_ROOT/mineconda.lock"
test -f "$PROJECT_ROOT/mods/jei.jar"

echo "[ci-smoke] run dry-run (client/server/both, loader-aware launcher detect)"
printf 'fake neoforge client launcher\n' > "$PROJECT_ROOT/.mineconda/dev/neoforge-client-launch.jar"
printf 'fake neoforge server launcher\n' > "$PROJECT_ROOT/.mineconda/dev/neoforge-server-launch.jar"

client_out="$("$BIN" --root "$PROJECT_ROOT" run --dry-run --java java --mode client)"
printf '%s\n' "$client_out"
printf '%s\n' "$client_out" | rg -q 'neoforge-client-launch.jar'

server_out="$("$BIN" --root "$PROJECT_ROOT" run --dry-run --java java --mode server)"
printf '%s\n' "$server_out"
printf '%s\n' "$server_out" | rg -q 'neoforge-server-launch.jar'

both_out="$("$BIN" --root "$PROJECT_ROOT" run --dry-run --java java --mode both)"
printf '%s\n' "$both_out"
printf '%s\n' "$both_out" | rg -q 'neoforge-server-launch.jar'
printf '%s\n' "$both_out" | rg -q 'neoforge-client-launch.jar'

echo "[ci-smoke] run --json (dry-run)"
run_json="$("$BIN" --root "$PROJECT_ROOT" run --dry-run --json --java java --mode both)"
printf '%s\n' "$run_json"
JSON_PAYLOAD="$run_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "run", payload
assert payload["dry_run"] is True, payload
assert payload["mode"] == "both", payload
assert payload["summary"]["launches"] == 2, payload
PY

echo "[ci-smoke] env install/use/list/which"
if [[ -d "$RUNTIME_SEED_DIR/payload" ]]; then
  echo "[ci-smoke] seed managed java from $RUNTIME_SEED_DIR"
  mkdir -p "$MINECONDA_HOME/runtimes/java/temurin"
  rm -rf "$MINECONDA_HOME/runtimes/java/temurin/21"
  cp -R "$RUNTIME_SEED_DIR" "$MINECONDA_HOME/runtimes/java/temurin/21"
  rm -f "$MINECONDA_HOME/runtimes/java/temurin/21/runtime.json"
fi
"$BIN" --root "$PROJECT_ROOT" env install 21 --use-for-project
env_list_out="$("$BIN" --root "$PROJECT_ROOT" env list)"
printf '%s\n' "$env_list_out"
printf '%s\n' "$env_list_out" | rg -q '^\* java 21 \(temurin\) -> '
env_which_out="$("$BIN" --root "$PROJECT_ROOT" env which)"
printf '%s\n' "$env_which_out"
printf '%s\n' "$env_which_out" | rg -q '^java 21 \(temurin\) -> '
JAVA_BIN="$(printf '%s\n' "$env_which_out" | sed 's/^.* -> //')"
JAVA_HOME="$(cd "$(dirname "$JAVA_BIN")/.." && pwd)"
mkdir -p "$(dirname "$RUNTIME_SEED_DIR")"
rm -rf "$RUNTIME_SEED_DIR"
cp -R "$MINECONDA_HOME/runtimes/java/temurin/21" "$RUNTIME_SEED_DIR"
"$ROOT_DIR/scripts/build-run-smoke-fixtures.sh" "$JAVA_BIN" "$PROJECT_ROOT" >/dev/null
printf 'smoke-stage=true\n' > "$PROJECT_ROOT/config/mineconda-run-stage.toml"
printf 'motd=mineconda smoke\n' > "$PROJECT_ROOT/server.properties"
printf 'eula=true\n' > "$PROJECT_ROOT/eula.txt"

echo "[ci-smoke] doctor after runtime install"
doctor_after_runtime="$("$BIN" --root "$PROJECT_ROOT" doctor)"
printf '%s\n' "$doctor_after_runtime"
printf '%s\n' "$doctor_after_runtime" | rg -q '\[ok\] managed runtime: java 21 \(temurin\) -> '

echo "[ci-smoke] run actual managed client/server/both"
rm -f "$PROJECT_ROOT/.mineconda/instances/dev/mineconda-smoke-client.txt"
rm -f "$PROJECT_ROOT/.mineconda/instances/dev/mineconda-smoke-server.txt"
rm -f "$PROJECT_ROOT/.mineconda/instances/dev-client/mineconda-smoke-client.txt"
rm -f "$PROJECT_ROOT/.mineconda/instances/dev-server/mineconda-smoke-server.txt"

client_real_out="$("$BIN" --root "$PROJECT_ROOT" run --mode client)"
printf '%s\n' "$client_real_out"
printf '%s\n' "$client_real_out" | rg -q 'MINECONDA_SMOKE_START role=client'
printf '%s\n' "$client_real_out" | rg -F "MINECONDA_SMOKE_JAVA_HOME=$JAVA_HOME"
printf '%s\n' "$client_real_out" | rg -F "MINECONDA_SMOKE_GAMEDIR=$PROJECT_ROOT/.mineconda/instances/dev"
test -f "$PROJECT_ROOT/.mineconda/instances/dev/mineconda-smoke-client.txt"
test -f "$PROJECT_ROOT/.mineconda/instances/dev/config/mineconda-run-stage.toml"
rg -q '^smoke-stage=true$' "$PROJECT_ROOT/.mineconda/instances/dev/config/mineconda-run-stage.toml"
rg -q '^motd=mineconda smoke$' "$PROJECT_ROOT/.mineconda/instances/dev/server.properties"
rg -q '^eula=true$' "$PROJECT_ROOT/.mineconda/instances/dev/eula.txt"

server_real_out="$("$BIN" --root "$PROJECT_ROOT" run --mode server --jvm-arg=-Dmineconda.smoke.server_sleep_ms=250)"
printf '%s\n' "$server_real_out"
printf '%s\n' "$server_real_out" | rg -q 'MINECONDA_SMOKE_START role=server'
printf '%s\n' "$server_real_out" | rg -q 'MINECONDA_SMOKE_ARG\[0\]=nogui'
printf '%s\n' "$server_real_out" | rg -F "MINECONDA_SMOKE_JAVA_HOME=$JAVA_HOME"
test -f "$PROJECT_ROOT/.mineconda/instances/dev/mineconda-smoke-server.txt"
test -f "$PROJECT_ROOT/.mineconda/instances/dev/config/mineconda-run-stage.toml"

both_real_out="$("$BIN" --root "$PROJECT_ROOT" run --mode both --jvm-arg=-Dmineconda.smoke.server_sleep_ms=5000)"
printf '%s\n' "$both_real_out"
printf '%s\n' "$both_real_out" | rg -q 'MINECONDA_SMOKE_START role=server'
printf '%s\n' "$both_real_out" | rg -q 'MINECONDA_SMOKE_START role=client'
test -f "$PROJECT_ROOT/.mineconda/instances/dev-server/mineconda-smoke-server.txt"
test -f "$PROJECT_ROOT/.mineconda/instances/dev-client/mineconda-smoke-client.txt"
test -f "$PROJECT_ROOT/.mineconda/instances/dev-server/config/mineconda-run-stage.toml"
test -f "$PROJECT_ROOT/.mineconda/instances/dev-client/config/mineconda-run-stage.toml"

echo "[ci-smoke] workspace run dry-run/actual"
WORKSPACE_RUN_ROOT="$WORK_ROOT/workspace-run"
"$BIN" --root "$WORKSPACE_RUN_ROOT" workspace init smoke-ws
"$BIN" --root "$WORKSPACE_RUN_ROOT" workspace add packs/client
"$BIN" --root "$WORKSPACE_RUN_ROOT" workspace add packs/server
"$BIN" --root "$WORKSPACE_RUN_ROOT" --member packs/client init smoke-client --minecraft 1.21.1 --loader neoforge --loader-version 21.1.227
"$BIN" --root "$WORKSPACE_RUN_ROOT" --member packs/server init smoke-server --minecraft 1.21.1 --loader neoforge --loader-version 21.1.227
printf 'fake workspace client launcher\n' > "$WORKSPACE_RUN_ROOT/packs/client/.mineconda/dev/neoforge-client-launch.jar"
printf 'fake workspace client launcher\n' > "$WORKSPACE_RUN_ROOT/packs/server/.mineconda/dev/neoforge-client-launch.jar"
workspace_run_dry="$("$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members run --dry-run --java java --mode client)"
printf '%s\n' "$workspace_run_dry"
printf '%s\n' "$workspace_run_dry" | rg -q 'workspace run: 2 members'
printf '%s\n' "$workspace_run_dry" | rg -q '==> packs/client'
printf '%s\n' "$workspace_run_dry" | rg -q '==> packs/server'
printf '%s\n' "$workspace_run_dry" | rg -q 'dry-run \[client\]:'
workspace_run_real="$("$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members run --java /usr/bin/true --mode client)"
printf '%s\n' "$workspace_run_real"
printf '%s\n' "$workspace_run_real" | rg -q 'workspace summary: ok=2 stale=0 failed=0'

echo "[ci-smoke] workspace sync/export/run --json"
"$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members lock >/dev/null
workspace_sync_json="$("$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members sync --check --json)"
JSON_PAYLOAD="$workspace_sync_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "sync", payload
assert payload["summary"]["members"] == 2, payload
assert payload["summary"]["exit_code"] == 0, payload
PY
workspace_run_json="$("$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members run --dry-run --json --java java --mode client)"
JSON_PAYLOAD="$workspace_run_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "run", payload
assert payload["summary"]["members"] == 2, payload
assert len(payload["members"]) == 2, payload
PY
workspace_export_json="$("$BIN" --root "$WORKSPACE_RUN_ROOT" --all-members export --format mods-desc --output dist/workspace-mods --json)"
JSON_PAYLOAD="$workspace_export_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "export", payload
assert payload["summary"]["members"] == 2, payload
assert len(payload["members"]) == 2, payload
PY

echo "[ci-smoke] remove local-only mod before strict mrpack export"
"$BIN" --root "$PROJECT_ROOT" remove jei --source local
"$BIN" --root "$PROJECT_ROOT" remove smoke-probe --source local

echo "[ci-smoke] export mrpack/mods-desc"
"$BIN" --root "$PROJECT_ROOT" export --format mrpack --output dist/mypack
cf_export_out="$("$BIN" --root "$PROJECT_ROOT" export --format curseforge --output dist/mypack-cf 2>&1)"
printf '%s\n' "$cf_export_out"
printf '%s\n' "$cf_export_out" | rg -q 'compatibility-oriented'
"$BIN" --root "$PROJECT_ROOT" export --format mods-desc --output dist/mods
test -f "$PROJECT_ROOT/dist/mypack.mrpack"
test -f "$PROJECT_ROOT/dist/mypack-cf.zip"
test -f "$PROJECT_ROOT/dist/mods.json"
rg -q '"declared_mods"' "$PROJECT_ROOT/dist/mods.json"
rg -q '"resolved_mods"' "$PROJECT_ROOT/dist/mods.json"
rg -q '"source": "modrinth"' "$PROJECT_ROOT/dist/mods.json"
unzip -p "$PROJECT_ROOT/dist/mypack.mrpack" modrinth.index.json | rg -q '"neoforge":"'
if unzip -p "$PROJECT_ROOT/dist/mypack.mrpack" modrinth.index.json | rg -q '"neoforge":"latest"'; then
  echo "[ci-smoke] failed: mrpack loader version must not be latest"
  exit 1
fi
unzip -p "$PROJECT_ROOT/dist/mypack-cf.zip" manifest.json | rg -q '"id":"neoforge-'
if unzip -p "$PROJECT_ROOT/dist/mypack-cf.zip" manifest.json | rg -q '"id":"neoforge-latest"'; then
  echo "[ci-smoke] failed: curseforge loader version must not be latest"
  exit 1
fi

echo "[ci-smoke] export --json"
export_json="$("$BIN" --root "$PROJECT_ROOT" export --format mods-desc --output dist/mods-json --json)"
printf '%s\n' "$export_json"
JSON_PAYLOAD="$export_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "export", payload
assert payload["format"] == "mods-desc", payload
assert payload["summary"]["exit_code"] == 0, payload
PY
test -f "$PROJECT_ROOT/dist/mods-json.json"

echo "[ci-smoke] import auto (local/url) + non-mod path sync"
command -v python3 >/dev/null
IMPORT_SAMPLE="$WORK_ROOT/import-sample.mrpack"
PROJECT_ROOT="$PROJECT_ROOT" IMPORT_SAMPLE="$IMPORT_SAMPLE" python3 <<'PY'
import json
import os
import tomllib
import zipfile

lock_path = os.path.join(os.environ["PROJECT_ROOT"], "mineconda.lock")
with open(lock_path, "rb") as fp:
    lock = tomllib.load(fp)

selected = None
for pkg in lock.get("packages", []):
    hashes = {entry["algorithm"]: entry["value"] for entry in pkg.get("hashes", [])}
    if (
        hashes.get("sha1")
        and hashes.get("sha512")
        and pkg.get("download_url", "").startswith("https://")
        and pkg.get("file_size")
    ):
        selected = (pkg, hashes)
        break

if selected is None:
    raise SystemExit("no package with sha1+sha512+https+file_size found in lock")

pkg, hashes = selected
loader_key = {
    "fabric": "fabric-loader",
    "forge": "forge",
    "neo-forge": "neoforge",
    "quilt": "quilt-loader",
}.get(lock["metadata"]["loader"]["kind"])
if not loader_key:
    raise SystemExit(f"unsupported loader kind in lock metadata: {lock['metadata']['loader']['kind']}")
index = {
    "formatVersion": 1,
    "game": "minecraft",
    "versionId": "1.0.0",
    "name": "ImportAutoPack",
    "summary": "smoke import sample",
    "dependencies": {
        "minecraft": lock["metadata"]["minecraft"],
        loader_key: lock["metadata"]["loader"]["version"],
    },
    "files": [
        {
            "path": "mods/imported-mod.jar",
            "hashes": {"sha1": hashes["sha1"], "sha512": hashes["sha512"]},
            "downloads": [pkg["download_url"]],
            "fileSize": pkg["file_size"],
            "env": {"client": "required", "server": "required"},
        },
        {
            "path": "config/imported/settings.jar",
            "hashes": {"sha1": hashes["sha1"], "sha512": hashes["sha512"]},
            "downloads": [pkg["download_url"]],
            "fileSize": pkg["file_size"],
            "env": {"client": "required", "server": "required"},
        },
    ],
}

with zipfile.ZipFile(os.environ["IMPORT_SAMPLE"], "w", compression=zipfile.ZIP_STORED) as zf:
    zf.writestr("modrinth.index.json", json.dumps(index, ensure_ascii=False))
    zf.writestr("overrides/config/from-import.toml", "from_import=true\n")
PY

IMPORT_LOCAL_ROOT="$WORK_ROOT/import-auto-local"
"$BIN" --root "$IMPORT_LOCAL_ROOT" import "$IMPORT_SAMPLE"
test -f "$IMPORT_LOCAL_ROOT/mineconda.toml"
test -f "$IMPORT_LOCAL_ROOT/mineconda.lock"
rg -q 'minecraft = "1.21.1"' "$IMPORT_LOCAL_ROOT/mineconda.toml"
rg -q 'source = "modrinth"' "$IMPORT_LOCAL_ROOT/mineconda.lock"
rg -q 'install_path = "config/imported/settings.jar"' "$IMPORT_LOCAL_ROOT/mineconda.lock"
"$BIN" --root "$IMPORT_LOCAL_ROOT" sync --offline --jobs 2 >/dev/null
test -f "$IMPORT_LOCAL_ROOT/mods/imported-mod.jar"
test -f "$IMPORT_LOCAL_ROOT/config/imported/settings.jar"
test -f "$IMPORT_LOCAL_ROOT/config/from-import.toml"
test -f "$IMPORT_LOCAL_ROOT/overrides/config/from-import.toml"

echo "[ci-smoke] import --json"
IMPORT_JSON_ROOT="$WORK_ROOT/import-auto-json"
import_json="$("$BIN" --root "$IMPORT_JSON_ROOT" import "$IMPORT_SAMPLE" --json)"
printf '%s\n' "$import_json"
JSON_PAYLOAD="$import_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "import", payload
assert payload["detected_format"] == "modrinth-mrpack", payload
assert payload["summary"]["exit_code"] == 0, payload
PY
test -f "$IMPORT_JSON_ROOT/mineconda.toml"
test -f "$IMPORT_JSON_ROOT/mineconda.lock"

echo "[ci-smoke] workspace import --json"
WORKSPACE_IMPORT_ROOT="$WORK_ROOT/workspace-import"
"$BIN" --root "$WORKSPACE_IMPORT_ROOT" workspace init import-ws
"$BIN" --root "$WORKSPACE_IMPORT_ROOT" workspace add packs/client
"$BIN" --root "$WORKSPACE_IMPORT_ROOT" workspace add packs/server
mkdir -p "$WORKSPACE_IMPORT_ROOT/imports/packs/client" "$WORKSPACE_IMPORT_ROOT/imports/packs/server"
cp "$IMPORT_SAMPLE" "$WORKSPACE_IMPORT_ROOT/imports/packs/client/client-pack.mrpack"
cp "$IMPORT_SAMPLE" "$WORKSPACE_IMPORT_ROOT/imports/packs/server/server-pack.mrpack"
workspace_import_json="$("$BIN" --root "$WORKSPACE_IMPORT_ROOT" --all-members import imports --json)"
printf '%s\n' "$workspace_import_json"
JSON_PAYLOAD="$workspace_import_json" python3 <<'PY'
import json
import os

payload = json.loads(os.environ["JSON_PAYLOAD"])
assert payload["command"] == "import", payload
assert payload["summary"]["members"] == 2, payload
assert payload["summary"]["failed"] == 0, payload
PY
test -f "$WORKSPACE_IMPORT_ROOT/packs/client/mineconda.toml"
test -f "$WORKSPACE_IMPORT_ROOT/packs/server/mineconda.toml"

echo "[ci-smoke] import auto rejects unsupported archive"
INVALID_IMPORT="$WORK_ROOT/import-unsupported.zip"
INVALID_IMPORT="$INVALID_IMPORT" python3 <<'PY'
import os
import zipfile

with zipfile.ZipFile(os.environ["INVALID_IMPORT"], "w", compression=zipfile.ZIP_STORED) as zf:
    zf.writestr("manifest.json", '{"minecraft":{"version":"1.21.1"}}')
PY
if "$BIN" --root "$WORK_ROOT/import-auto-invalid" import "$INVALID_IMPORT" >"$WORK_ROOT/import-invalid.out" 2>&1; then
  echo "[ci-smoke] failed: unsupported import unexpectedly succeeded"
  exit 1
fi
rg -q 'currently only Modrinth \.mrpack is supported' "$WORK_ROOT/import-invalid.out"

REMOTE_MRPACK_URL="$(python3 <<'PY'
import json
import sys
import urllib.parse
import urllib.request

queries = ["test", "optimization", "fabric"]
for query in queries:
    search_params = urllib.parse.urlencode({
        "query": query,
        "limit": 20,
        "facets": '[["project_type:modpack"]]',
    })
    search_url = f"https://api.modrinth.com/v2/search?{search_params}"
    with urllib.request.urlopen(search_url, timeout=20) as response:
        payload = json.load(response)

    candidates = []
    for hit in payload.get("hits", []):
        project_id = hit.get("project_id")
        if not project_id:
            continue
        versions_url = f"https://api.modrinth.com/v2/project/{project_id}/version"
        with urllib.request.urlopen(versions_url, timeout=20) as response:
            versions = json.load(response)
        for version in versions:
            for file in version.get("files", []):
                url = file.get("url")
                name = file.get("filename", "")
                size = int(file.get("size", 0))
                if not url or not url.startswith("https://"):
                    continue
                if not name.endswith(".mrpack"):
                    continue
                candidates.append((size, url))

    if candidates:
        candidates.sort(key=lambda item: item[0] if item[0] > 0 else 2**63)
        print(candidates[0][1])
        sys.exit(0)

raise SystemExit("failed to discover a remote mrpack URL from Modrinth API")
PY
)"
IMPORT_URL_ROOT="$WORK_ROOT/import-auto-url"
MINECONDA_NO_PROXY=1 "$BIN" --root "$IMPORT_URL_ROOT" import "$REMOTE_MRPACK_URL"
test -f "$IMPORT_URL_ROOT/mineconda.toml"
test -f "$IMPORT_URL_ROOT/mineconda.lock"

if [[ ! -f "$PROJECT_ROOT/vendor/jei.jar" ]]; then
  mkdir -p "$PROJECT_ROOT/vendor"
  printf 'restored jei jar for optional s3 smoke\n' > "$PROJECT_ROOT/vendor/jei.jar"
fi

if [[ "${MINECONDA_ENABLE_S3_SMOKE:-0}" == "1" ]]; then
  echo "[ci-smoke] s3 smoke via ssh+wsl (optional)"
  "$ROOT_DIR/scripts/s3-smoke-wsl.sh" "$BIN" "$PROJECT_ROOT"
else
  echo "[ci-smoke] skip optional s3 smoke (set MINECONDA_ENABLE_S3_SMOKE=1 to enable)"
fi

echo "[ci-smoke] cache purge"
"$BIN" --root "$PROJECT_ROOT" cache purge
"$BIN" --root "$PROJECT_ROOT" cache ls >/dev/null

echo "[ci-smoke] done: $PROJECT_ROOT"
