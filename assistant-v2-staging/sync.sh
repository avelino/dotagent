#!/usr/bin/env bash
# sync.sh — copy the staged assistant-v2 agents into ~/dotfiles and clean
# up the files the daemon harness replaced. Run from the dotagent repo root:
#
#   bash assistant-v2-staging/sync.sh          # copy + remove replaced files
#   bash assistant-v2-staging/sync.sh --check  # dry run
#
# After running: cd ~/dotfiles && git add -A, then darwin-rebuild switch
# (new agent dirs need the home.file entries), then dotagent reload.

set -euo pipefail

DOTFILES="$HOME/dotfiles/modules/home-manager/dotagent/agents"
STAGING="$(cd "$(dirname "$0")" && pwd)"
DRY=0
[[ "${1:-}" == "--check" ]] && DRY=1

copy() {
	local src="$1" dst="$2"
	if [[ $DRY -eq 1 ]]; then
		echo "would copy: $src -> $dst"
	else
		echo "copy: $src -> $dst"
		cp "$src" "$dst"
	fi
}

remove() {
	if [[ -e "$1" ]]; then
		if [[ $DRY -eq 1 ]]; then
			echo "would remove: $1"
		else
			echo "remove: $1 (superseded by the daemon harness)"
			rm "$1"
		fi
	else
		echo "already gone: $1"
	fi
}

# -- telegram-assistant v2 (thin dispatcher) --------------------------------
mkdir -p "$DOTFILES/telegram-assistant"
copy "$STAGING/telegram-assistant/agent.toml" "$DOTFILES/telegram-assistant/agent.toml"
copy "$STAGING/telegram-assistant/agent.sh" "$DOTFILES/telegram-assistant/agent.sh"
[[ $DRY -eq 0 ]] && chmod +x "$DOTFILES/telegram-assistant/agent.sh"
copy "$STAGING/telegram-assistant/prompt.md" "$DOTFILES/telegram-assistant/prompt.md"

# Superseded: uuid5/generation markers, warm pool, shell-side memory — all
# daemon-owned now.
remove "$DOTFILES/telegram-assistant/agent.fish"
remove "$DOTFILES/telegram-assistant/session-pool.py"
remove "$DOTFILES/telegram-assistant/memory.py"

# -- proactive secretary (new) ----------------------------------------------
for agent in memory-consolidate morning-briefing follow-up-sweeper; do
	mkdir -p "$DOTFILES/$agent"
	for f in "$STAGING/$agent"/*; do
		copy "$f" "$DOTFILES/$agent/$(basename "$f")"
	done
done

cat <<'EOF'

next steps (in ~/dotfiles):
  git add -A                      # flakes only see tracked files
  darwin-rebuild switch --flake . # new agent dirs need home.file entries
  dotagent reload                 # daemon picks up manifests + schedules

and rebuild/install the daemon first if you have not:
  cd ~/projects/avelino/dotagent && cargo build --release -p dotagent
  # install per your usual path (cargo install --path or the Homebrew formula)
EOF
