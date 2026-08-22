#!/usr/bin/env fish
# morning-briefing — agenda + pendências + falhas da noite, em uma mensagem.
#
# Coletores determinísticos (sem LLM) em paralelo, uma passada de claude pra
# compactar no tom do Avelino, stdout vira o post do notifier.
# Fonte morta = seção pulada com uma linha curta, não abort.

set -l script_dir (dirname (status filename))
set -l tmp (mktemp -d)
trap 'rm -rf $tmp' EXIT

# -- Agenda (hoje): proxy gws, melhor esforço -------------------------------
if mcp gws agenda --days 1 >/dev/null 2>&1
    mcp gws agenda --days 1 > $tmp/agenda.txt 2>/dev/null
    or echo "(agenda indisponível)" > $tmp/agenda.txt
else
    echo "(agenda indisponível)" > $tmp/agenda.txt
end

# -- Pendências: TODOs da memória (journals projetados em .md) --------------
grep -rh "TODO até" ~/.config/dotagent/outl/journals/ 2>/dev/null | sort -u > $tmp/todos.txt
or echo "(sem pendências anotadas)" > $tmp/todos.txt

# -- Falhas da noite: heartbeats com erro nas últimas 12h --------------------
dotagent status --json 2>/dev/null | jq -r \
    '.agents[] | select(.health == "failing" or .health == "degraded") | "- \(.name): \(.health)"' 2>/dev/null > $tmp/failures.txt
or true
test -s $tmp/failures.txt; or echo "(nenhuma)" > $tmp/failures.txt

# -- Compacta ----------------------------------------------------------------
# append, not replace: the default system prompt is what keeps this headless
# run terse. Replacing it lets the personal CLAUDE.md skills take over and
# the briefing comes back wrapped in meta-commentary.
cat $tmp/agenda.txt $tmp/todos.txt $tmp/failures.txt | claude -p \
    --model haiku \
    --strict-mcp-config \
    --settings '{"hooks":{}}' \
    --append-system-prompt (cat $script_dir/prompt.md | string collect) \
    -p - 2>/dev/null
