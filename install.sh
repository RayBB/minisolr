#!/bin/sh
# Switch an Open Library checkout between real Solr and minisolr-rs (dev only).
#   curl -fsSL https://raw.githubusercontent.com/RayBB/minisolr-rs/main/install.sh | sh          # Rust image (default)
#   curl -fsSL https://raw.githubusercontent.com/RayBB/minisolr-rs/main/install.sh | sh -s off   # back to Solr
# Run it from the Open Library checkout (or set OL_DIR). It downloads one compose file to
# ~/.minisolr/ and sets COMPOSE_FILE in OL's git-ignored .env; nothing else in the checkout changes.
set -eu

mode="${1:-rust}"
ol_dir="${OL_DIR:-$PWD}"
env_file="$ol_dir/.env"

[ -f "$ol_dir/compose.yaml" ] || { echo "No compose.yaml in $ol_dir: run from the Open Library checkout or set OL_DIR." >&2; exit 1; }

drop_line() {
    [ -f "$env_file" ] || return 0
    grep -v '^COMPOSE_FILE=' "$env_file" > "$env_file.tmp" || true
    mv "$env_file.tmp" "$env_file"
}

if [ "$mode" = off ]; then
    drop_line
    echo "Back to real Solr. Run: docker compose up -d --force-recreate solr   (then make reindex-solr)"
    exit 0
fi

case "$mode" in
    rust) repo=minisolr-rs ;;
    *) echo "Usage: install.sh [rust|off]" >&2; exit 1 ;;
esac

dest="$HOME/.minisolr/$repo.compose.yaml"
mkdir -p "$HOME/.minisolr"
curl -fsSL -o "$dest" "https://raw.githubusercontent.com/RayBB/$repo/main/compose.ghcr.yaml"

files="compose.yaml"
[ -f "$ol_dir/compose.override.yaml" ] && files="$files:compose.override.yaml"
drop_line
echo "COMPOSE_FILE=$files:$dest" >> "$env_file"

echo "Using $repo. Now run:"
echo "  docker compose up -d && docker compose run --rm home make reindex-solr"
echo "Undo with: curl -fsSL https://raw.githubusercontent.com/RayBB/minisolr-rs/main/install.sh | sh -s off"
