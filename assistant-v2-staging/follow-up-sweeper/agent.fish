#!/usr/bin/env fish
# follow-up-sweeper — ping pendências vencidas, no máx. 1x/dia por item.
#
#stdout vira o post do notifier; nada vencido = saída vazia + exit 0
# (notifier telegram com corpo vazio não posta nada relevante pro chat).
# Dedup: um marker file por (item, dia) em state/follow-up-sweeper.

set -l state_dir ~/.config/dotagent/state/follow-up-sweeper
mkdir -p $state_dir
set -l today (date +%Y-%m-%d)
set -l today_num (date +%Y%m%d)

# TODOs com data absoluta, dedup por texto exato entre journals.
set -l todos (grep -rh "TODO até" ~/.config/dotagent/outl/journals/ 2>/dev/null | sort -u)
test (count $todos) -gt 0; or exit 0

set -l due
for line in $todos
    set -l m (string match -r 'TODO até ([0-9]{4}-[0-9]{2}-[0-9]{2})' -- $line)
    test (count $m) -ge 2; or continue
    set -l due_num (string replace -a '-' '' -- $m[2])
    # vencido ou vence hoje: data <= hoje
    test $due_num -le $today_num; or continue

    set -l key (printf '%s' $line | shasum | cut -c1-12)
    if not test -e $state_dir/$key.$today
        set -a due $line
        touch $state_dir/$key.$today
    end
end

# markers velhos saem depois de 2 dias
find $state_dir -type f ! -name "*.$today" -mtime +2 -delete 2>/dev/null

test (count $due) -gt 0; or exit 0

printf 'pendências pra hoje:\n'
for line in $due
    # the journal line arrives with its markdown bullet; strip it.
    printf '- %s\n' (string replace -r '^\s*-\s+' '' -- $line)
end
