---
name: dotagent:port-fish-agent
description: Migrate an agent from the Fish-based `lib/agent.fish` framework (in avelino/dotfiles) to dotagent. Use when the user says "porta X pro dotagent", "migra Y", "transforma meta.json em agent.toml". Produces a working agent.toml that points at the existing agent.fish without rewriting it.
---

# Skill: port-fish-agent

Migra agent do framework Fish (avelino/dotfiles `lib/agent.fish`) pra
dotagent. **Não reescreve o `agent.fish`** — só cria `agent.toml` e remove
o que dotagent agora dá grátis.

## Quando usar

- "migra `linkedin-hot-take` pro dotagent"
- "porta `hourly-briefing`"
- "quero que `finops-weekly` rode pelo dotagent"

## Inputs necessários

- Path da pasta do agent no dotfiles (`agents/<nome>/`)
- O `meta.json` daquela pasta
- O `agent.fish` (lê pra ver dependências de framework)

## Como aplicar

### 1. Criar `agent.toml` a partir do `meta.json`

Mapping direto:

| meta.json                              | agent.toml                                    |
|----------------------------------------|-----------------------------------------------|
| `"name": "x"`                          | `[agent] name = "x"`                          |
| `"monitor": true`                      | `[agent] monitor = true`                      |
| `"timeout_seconds": N`                 | `[agent] timeout_seconds = N`                 |
| top-level `"max_retries"`              | `[defaults] max_retries = N`                  |
| top-level `"retry_backoff_minutes"`    | `[defaults] retry_backoff_minutes = [...]`    |
| top-level `"stale_after_minutes"`      | `[defaults] stale_after_minutes = N`          |
| `schedules[]` cron-style               | `[[schedules]] type = "cron"` + weekdays/hours/minute |
| `schedules[].interval_minutes`         | `[[schedules]] type = "interval"` + interval_minutes |
| `schedules[].max_retries`              | per-schedule override (mesmo campo)           |

E **adiciona** o que o meta.json não tinha:

```toml
[run]
command = "fish"
args = ["./agent.fish"]
```

### 2. Remover do `agent.fish` o que dotagent injeta

dotagent injeta env vars (`AGENT_NAME`, `AGENT_HOME`, `AGENT_TMPDIR`,
`AGENT_DRY_RUN`, `AGENT_SCHEDULE_ID`, `AGENT_START_EPOCH`, `AGENT_ARGV`,
`AGENT_HEARTBEAT_FILE`) — então `agent_init` do framework pode ser
substituído por leitura direta dessas vars.

Estratégia recomendada (sem tocar no agent.fish hoje): criar
`~/dotfiles/lib/agent-shim.fish` que **lê env vars do dotagent** e expõe os
mesmos nomes que `lib/agent.fish` usava:

```fish
# lib/agent-shim.fish
set -g AGENT_NAME $AGENT_NAME
set -g AGENT_HOME $AGENT_HOME
set -g AGENT_TMPDIR $AGENT_TMPDIR
set -g AGENT_DRY_RUN $AGENT_DRY_RUN
set -g AGENT_ARGV (string split " " (echo $AGENT_ARGV | jq -r '.[]?'))
# ... etc
```

O `agent.fish` muda só a linha 1:

```diff
-source (status dirname)"/../../lib/agent.fish"
-agent_init "meu-agent" $argv
+source (status dirname)"/../../lib/agent-shim.fish"
```

### 3. Remover o que dotagent agora faz

| O que o agent.fish fazia | Substituído por |
|---|---|
| `agent_heartbeat_start` / `_agent_heartbeat_finish` | dotagent escreve heartbeat com mesmo shape |
| WARP check inline | `[[preflight]] plugin = "preflight-warp"` |
| `agent_output_imessage` | `[[notifiers]] driver = "imessage"` (built-in, sem plugin) |
| `roam_publish` | `[[on_success]] plugin = "sink-roam"` (quando estiver implementado) |

Pra MVP, mantém as chamadas no `agent.fish` (não quebra nada). Move pra
plugin/notifier gradualmente.

### 4. Notifiers + sinks

Adiciona ao `agent.toml`:

```toml
[[notifiers]]
driver = "imessage"
to = "+5511999999999"
rate_limit_minutes = 60
events = ["given_up"]
```

### 5. Symlinkar pra ~/.config/dotagent/agents/

```bash
mkdir -p ~/.config/dotagent/agents
ln -s ~/dotfiles/agents/<nome> ~/.config/dotagent/agents/<nome>
```

(Ou copia, se preferir desacoplar.)

### 6. Validar

```bash
dotagent doctor                                  # detecta o agent novo
dotagent run <nome> --schedule <id> --dry-run    # roda sem efeito colateral
dotagent run <nome> --schedule <id>              # roda real
cat ~/.local/state/dotagent/agents/<nome>/<slug>.heartbeat.json | jq
```

Compara com o heartbeat antigo (`~/.local/state/agents/...`) — shape deve
casar.

### 7. Remover do cron.nix antigo (só depois de validação)

Depois de 3-5 dias rodando paralelo (Fish e dotagent escrevendo no mesmo
heartbeat), remove a entrada do `modules/home-manager/cron.nix` e roda
`darwin-rebuild switch`.

## Princípios

- **Não reescreve agent.fish** no primeiro corte. Move pra plugin
  gradualmente.
- **Heartbeat path idêntico**. dotagent reusa o path do Fish framework de
  propósito (`~/.local/state/dotagent/agents/...`) — então as duas
  ferramentas convivem lendo o mesmo arquivo (note: dotagent escreve em
  `dotagent/agents/`, Fish em `agents/`, mas convergem na próxima rev).
- **Plista do agent NÃO é mais gerada per-schedule**. Quem dispara é o
  daemon `run.avelino.dotagent`. `dotagent install <nome>` é no-op no
  fluxo daemon — apenas registra o agent no índice.

## Anti-patterns

- ❌ Reescrever lógica de coleta de dados em Rust. O agent.fish funciona —
  deixa quieto. O ganho do dotagent é orquestração, não reimplementação.
- ❌ Migrar todos os agents de uma vez. Vai um por vez, valida 3-5 dias,
  remove do cron.nix antigo, próximo.
- ❌ Mudar `IMESSAGE_TO` durante migração. Use o número idêntico no
  `[[on_failure]]` do manifest pra reduzir variáveis.
