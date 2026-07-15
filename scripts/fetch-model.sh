#!/bin/bash
# Fetch the learned embedder (all-MiniLM-L6-v2, ~90MB) into the default model
# dir. Without it, rsd falls back to the hash-projection embedder.
set -euo pipefail
DIR="${RSD_MODEL_DIR:-$HOME/.cache/rsd/models/minilm}"
mkdir -p "$DIR"
cd "$DIR"
for f in config.json tokenizer.json model.safetensors; do
  [ -f "$f" ] || curl -L --fail -o "$f" \
    "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/$f"
done
echo "model ready at $DIR"
