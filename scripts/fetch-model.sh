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
echo "embedding model ready at $DIR"

# Whisper model for A/V transcription (opt-in: RSD_TRANSCRIBE=1). ~148MB.
WDIR="${RSD_WHISPER_DIR:-$HOME/.cache/rsd/models/whisper}"
mkdir -p "$WDIR"
if [ ! -f "$WDIR/ggml-base.en.bin" ]; then
  curl -L --fail -o "$WDIR/ggml-base.en.bin" \
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin"
fi
echo "whisper model ready at $WDIR"
