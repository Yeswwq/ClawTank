#!/usr/bin/env bash
# Packages clawtank-mcp as an MCP Bundle (.mcpb) for Claude Desktop and the MCP Registry,
# and writes the matching registry entry (server.json) next to it.
#
# Usage: scripts/package-mcpb.sh <version> <path/to/clawtank-mcp> <out.mcpb>
set -euo pipefail

version="$1"
binary="$2"
out="$3"
[[ -x "$binary" ]] || { echo "error: $binary not found or not executable" >&2; exit 1; }
raw_archs="$(lipo -archs "$binary")"
arch_key="$(printf '%s\n' "$raw_archs" | tr ' ' '\n' | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')"
case "$arch_key" in
    arm64|x86_64) arch="$arch_key" ;;
    "arm64 x86_64") arch=universal ;;
    *) echo "error: unsupported MCP binary architecture: $raw_archs" >&2; exit 1 ;;
esac
root="$(cd "$(dirname "$0")/.." && pwd)"
out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
repo_url="$(git remote get-url origin 2>/dev/null || true)"
case "$repo_url" in
    git@github.com:*) repo_url="https://github.com/${repo_url#git@github.com:}" ;;
    ssh://git@github.com/*) repo_url="https://github.com/${repo_url#ssh://git@github.com/}" ;;
    https://github.com/*) ;;
    *) repo_url="https://github.com/HMAKT99/ClawTank" ;;
esac
repo_url="${repo_url%.git}"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/server"
cp "$binary" "$stage/server/clawtank-mcp"
cp "$root/LICENSE" "$stage/LICENSE"
python3 -I - "$root/packaging/mcpb/manifest.json" "$stage/manifest.json" "$version" <<'EOF'
import json, sys
src, dst, version = sys.argv[1:]
m = json.load(open(src))
m["version"] = version
json.dump(m, open(dst, "w"), indent=2, ensure_ascii=False)
EOF

# Fixed timestamps: the same binary always gives the same bundle (and fileSha256).
find "$stage" -exec touch -h -t 202001010000 {} +
rm -f "$out"
(cd "$stage" && zip -qrX "$out" manifest.json LICENSE server)

# The bundle must start the server and list its tools without the ClawTank app running.
check="$(mktemp -d)"
trap 'rm -rf "$stage" "$check"' EXIT
unzip -q "$out" -d "$check"
tools="$(printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"package-mcpb","version":"0"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' |
    CLAWTANK_AGENT_TOKEN=ct_package_check "$check/server/clawtank-mcp" 2>/dev/null |
    python3 -I -c 'import sys, json; print(sum(len(json.loads(l).get("result", {}).get("tools", [])) for l in sys.stdin))')"
[[ "$tools" -gt 0 ]] || { echo "error: the bundled clawtank-mcp listed no tools" >&2; exit 1; }

sha="$(shasum -a 256 "$out" | cut -d' ' -f1)"
python3 -I - "$root/server.json" "$(dirname "$out")/server.json" "$version" "$(basename "$out")" "$sha" "$repo_url" <<'EOF'
import json, sys
from urllib.parse import urlparse
src, dst, version, name, sha, repo_url = sys.argv[1:]
s = json.load(open(src))
s["version"] = version
repository = urlparse(repo_url).path.strip("/").removesuffix(".git")
owner = repository.split("/")[0]
s["name"] = f"io.github.{owner}/clawtank"
s["repository"]["url"] = repo_url
s["websiteUrl"] = f"{repo_url}/blob/main/docs/agent-api.md"
pkg = s["packages"][0]
pkg["identifier"] = f"{repo_url}/releases/download/v{version}/{name}"
pkg["fileSha256"] = sha
json.dump(s, open(dst, "w"), indent=2, ensure_ascii=False)
open(dst, "a").write("\n")
EOF
echo "$out ($arch, $tools tools, sha256 $sha)"
