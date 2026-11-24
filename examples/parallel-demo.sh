#!/usr/bin/env bash
set -euo pipefail

BIN="${AGENT_BOX_BIN:-agent-box}"
OUT_DIR="${TMPDIR:-/tmp}/agent-box-demo-$$"
mkdir -p "$OUT_DIR"
BOX1="demoa$$"
BOX2="demob$$"

cleanup() {
  "$BIN" down "$BOX1" >/dev/null 2>&1 || true
  "$BIN" down "$BOX2" >/dev/null 2>&1 || true
  rm -rf "$OUT_DIR"
}
trap cleanup EXIT

"$BIN" up "$BOX1" --blank --cua off --url https://example.com >"$OUT_DIR/b1.json" &
P1=$!
"$BIN" up "$BOX2" --blank --cua off --url https://www.iana.org/domains/reserved >"$OUT_DIR/b2.json" &
P2=$!
wait "$P1" "$P2"

CDP1=$("$BIN" cdp "$BOX1")
CDP2=$("$BIN" cdp "$BOX2")
curl -fsS "$CDP1/json/version" >/dev/null
curl -fsS "$CDP2/json/version" >/dev/null

"$BIN" shot "$BOX1" "$OUT_DIR/b1.png" >/dev/null
"$BIN" shot "$BOX2" "$OUT_DIR/b2.png" >/dev/null
"$BIN" ls
