#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <java-bin> <project-root>" >&2
  exit 2
fi

JAVA_BIN="$1"
PROJECT_ROOT="$2"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_FILE="$ROOT_DIR/scripts/fixtures/run/MinecondaSmokeMain.java"
BUILD_DIR="$PROJECT_ROOT/.mineconda/test-fixtures/run"
CLASS_DIR="$BUILD_DIR/classes"
MANIFEST_PATH="$BUILD_DIR/MANIFEST.MF"
CLIENT_JAR="$PROJECT_ROOT/.mineconda/dev/neoforge-client-launch.jar"
SERVER_JAR="$PROJECT_ROOT/.mineconda/dev/neoforge-server-launch.jar"

JAVA_HOME="$(cd "$(dirname "$JAVA_BIN")/.." && pwd)"
JAVAC="$JAVA_HOME/bin/javac"
JAR="$JAVA_HOME/bin/jar"

if [[ ! -x "$JAVAC" ]]; then
  echo "javac not found at $JAVAC" >&2
  exit 1
fi
if [[ ! -x "$JAR" ]]; then
  echo "jar not found at $JAR" >&2
  exit 1
fi

rm -rf "$BUILD_DIR"
mkdir -p "$CLASS_DIR" "$(dirname "$CLIENT_JAR")"
printf 'Main-Class: MinecondaSmokeMain\n' > "$MANIFEST_PATH"

"$JAVAC" -d "$CLASS_DIR" "$SOURCE_FILE"
"$JAR" cfm "$CLIENT_JAR" "$MANIFEST_PATH" -C "$CLASS_DIR" .
cp "$CLIENT_JAR" "$SERVER_JAR"

echo "client_jar=$CLIENT_JAR"
echo "server_jar=$SERVER_JAR"
