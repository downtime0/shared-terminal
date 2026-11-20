#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cd "$repo_dir"

for tool in cargo install; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Error: required command not found: %s\n' "$tool" >&2
        exit 1
    fi
done

if command -v codex >/dev/null 2>&1 && ! command -v python3 >/dev/null 2>&1; then
    printf 'Error: python3 is required to check the existing Codex MCP entry.\n' >&2
    exit 1
fi

cargo build --release
cargo install --path "$repo_dir" --force

man_dir="$HOME/.local/share/man/man1"
install -Dm644 "$repo_dir/man/shared-terminal.1" \
    "$man_dir/shared-terminal.1"

# `codex mcp add` has no force option, so remove the existing entry first.
if command -v codex >/dev/null 2>&1; then
    if codex mcp list --json | python3 -c '
import json
import sys

servers = json.load(sys.stdin)
sys.exit(0 if any(server.get("name") == "shared-terminal" for server in servers) else 1)
    '; then
        codex mcp remove shared-terminal
    fi

    codex mcp add shared-terminal -- "$HOME/.cargo/bin/shared-terminal" mcp
else
    printf 'Codex CLI not found; skipping MCP registration.\n'
fi

printf '\nInstalled shared-terminal executable and man page.\n'
printf 'Man page: %s\n' "$man_dir/shared-terminal.1"
if command -v codex >/dev/null 2>&1; then
    printf 'Registered shared-terminal with Codex; restart Codex to load it.\n'
else
    printf 'Codex CLI was not found; MCP registration was skipped.\n'
fi
