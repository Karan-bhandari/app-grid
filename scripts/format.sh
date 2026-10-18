#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "Formatting Rust code (cargo fmt)..."
cargo fmt --manifest-path src-tauri/Cargo.toml

echo "Formatting TypeScript, CSS, HTML, JSON, and Markdown (prettier)..."
npx --no-install prettier --write \
  "src-ui/**/*.{ts,css,html}" \
  "package.json" \
  "tsconfig.json" \
  "vite.config.ts" \
  "src-tauri/tauri.conf.json" \
  "src-tauri/capabilities/*.json" \
  ".prettierrc.json" \
  ".agents/*.json" \
  "*.md" \
  ".github/**/*.yml"

echo "All files formatted successfully."
