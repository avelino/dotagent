# CLAUDE.md — guia pra desenvolver dotagent

Este arquivo orienta Claude Code (e outros agentes LLM) a trabalhar bem nesse
repositório desde o dia zero. **Leia inteiro antes de propor mudanças.**

## O que é o dotagent

Orquestrador poliglota de agents. Você escreve o agent na linguagem que quiser
(Fish, Python, Go, Rust, binário, shell script — qualquer coisa que leia env
vars e exit code). dotagent cuida de:

1. **Manifest** — `agent.toml` descreve identidade, comando, schedule, notifiers, plugins
2. **Scheduling adaptativo** — daemon único que dorme até o próximo evento
3. **Heartbeat** — estado por execução em `~/.config/dotagent/state/`
4. **Notifiers built-in** — `desktop`, `imessage`, `slack`, `ntfy`, `pushover` rodam dentro do daemon (crate `dotagent-notify`)
5. **Plugins** — preflight + sinks + notifiers de terceiros via subprocess + JSON stdio
6. **Unit gen** — gera plist (macOS) / systemd unit (Linux) pro daemon
7. **Health states** — `ok | degraded | failing | stale` por (agent, schedule)

Saiba o que o dotagent **não** é: não é runtime de agents, não é SDK, não é
proxy MCP, não é AI. É um scheduler + supervisor.

## Documentos canônicos (leia ANTES de mexer)

**Conceito + guia (humanos):**

- [`docs/concepts/agents.md`](docs/concepts/agents.md) — o que é agent, patterns, extending,
  connecting
- [`docs/concepts/plugins.md`](docs/concepts/plugins.md) — o que é plugin, como usar, como criar

**Referência (specs):**

- [`docs/reference/agent-spec.md`](docs/reference/agent-spec.md) — schema formal de `agent.toml`
- [`docs/reference/plugin-protocol.md`](docs/reference/plugin-protocol.md) — protocolo formal de
  subprocess + JSON stdio
- [`docs/security/threat-model.md`](docs/security/threat-model.md) — modelo de ameaças
- [`docs/guides/migrating-from-fish.md`](docs/guides/migrating-from-fish.md) — guia de
  migração da Fish framework
- [`docs/guides/observability.md`](docs/guides/observability.md) — logs estruturados,
  rotation, OTLP export pra Honeycomb / Tempo / Jaeger / Datadog

Se sua mudança contradiz qualquer um desses, **atualize o doc primeiro** —
schema e protocolo são contrato público.

## Arquitetura do repo

```
crates/                  # orchestrator (workspace de crates)
  dotagent/              # CLI binary — entry point
  dotagent-core/         # types: Manifest, Schedule, Heartbeat, WindowState, Config
  dotagent-scheduler/    # funções PURAS — sem IO, sem clock, totalmente testável
  dotagent-runner/       # spawn + timeout + heartbeat lifecycle + env injection
  dotagent-state/        # filesystem state (atomic write + flock) + paths
  dotagent-notify/       # notifiers built-in (desktop / imessage / slack / ntfy / pushover)
  dotagent-plugin/       # PluginClient (subprocess + JSON)
  dotagent-telemetry/    # tracing + JSON file rotation + OTLP export + retention
  dotagent-unit-gen/     # launchd plist + systemd unit generation

plugins/                 # plugins oficiais (cada um seu binário) — só preflight e sink
  preflight-{warp,cmd}/
  sink-{roam,file}/

examples/                # agents minimais (hello-*) + casos reais (disk-alert)

docs/
  concepts/              # agents.md, plugins.md (guias humanos)
  reference/             # agent-spec.md, plugin-protocol.md (specs formais)
  plugins/               # uma doc por plugin built-in
  guides/                # migrating-from-fish.md, observability.md
  security/              # threat-model.md
```

### Regra de ouro: a quem pertence cada coisa

| Mudança | Vai onde |
|---|---|
| Novo tipo compartilhado | `dotagent-core` |
| Função de tempo / agendamento | `dotagent-scheduler` (sem IO!) |
| IO de filesystem (state) | `dotagent-state` |
| Spawn de subprocess | `dotagent-runner` |
| Geração de plist/systemd | `dotagent-unit-gen` |
| Setup de logs / OTel / retention | `dotagent-telemetry` |
| Novo subcomando CLI | `crates/dotagent/src/commands/` |
| Novo notifier built-in (driver) | `dotagent-notify/src/drivers/` |
| Novo preflight/sink | novo crate em `plugins/` |
| Novo notifier de terceiro (não built-in) | novo crate em `plugins/` (kind = `notify`) |
| Mudança no shape do `agent.toml` | `dotagent-core/src/manifest.rs` + `docs/reference/agent-spec.md` |
| Mudança no protocolo de plugin | `dotagent-plugin/src/lib.rs` + `docs/reference/plugin-protocol.md` |
| Novo campo em `config.toml` | `dotagent-core/src/config.rs` + `docs/guides/observability.md` |

Se você está prestes a colocar IO no `dotagent-scheduler` — pare. Tem teste
unitário lá dependendo dessa pureza.

## Diagramas — sempre Mermaid

**Nunca** desenhe diagramas em ASCII (box-drawing `┌─┐│└┘`, setas
`──▶`, blocos `+--+`). GitHub renderiza Mermaid nativamente em qualquer
`.md` — use isso.

```markdown
\`\`\`mermaid
flowchart LR
    A[collect] --> B[transform] --> C[deliver]
\`\`\`
```

Tipos comuns:

- `flowchart LR` / `flowchart TD` — pipelines, dependências
- `sequenceDiagram` — interação entre processos (daemon ↔ plugin)
- `stateDiagram-v2` — máquinas de estado (health states, retry loop)
- `graph` — relações arbitrárias

Razão: ASCII quebra em terminais sem fonte monoespaçada, não escala
quando o diagrama cresce, e é miserável de editar. Mermaid é texto puro,
diff-friendly, e renderiza em GitHub / VS Code / Obsidian sem plugin.

## Convenções Rust

Alinhado com `~/.claude/CLAUDE.md` do Avelino (hm_rustrules.md):

- **Edition 2021** · **rustc stable** · **resolver = "2"**
- **async**: `tokio` com `full` no binary, features mínimas em libs
- **CLI**: `clap` derive
- **Serialização**: `serde` + `serde_json` + `toml`
- **HTTP** (plugins): `reqwest` com `rustls-tls`, **nunca** `default-features = true`
- **Logging**: `tracing` + `tracing-subscriber`
- **Erros**: `anyhow` no binary (`crates/dotagent/`, plugins), `thiserror` nas libs
- **IDs/tempo**: `chrono` (não `time`)
- **Sem `unsafe`** exceto onde estritamente necessário (já tem em `kill(2)` no runner — `// SAFETY:` comment obrigatório)

### Versionamento de deps

Centralizado em `[workspace.dependencies]` do `Cargo.toml` raiz. **Crates do
workspace usam `crate.workspace = true`**, nunca repetem versão.

Pra adicionar dep nova:
1. Adicione em `[workspace.dependencies]` com `version = "X"` + features
2. Use `crate.workspace = true` no Cargo.toml da crate que precisa

### Naming

- Crates internas: `dotagent-<area>` (kebab-case) — ex: `dotagent-notify`
- Plugins: `dotagent-plugin-<kind>-<name>` (ex: `dotagent-plugin-sink-roam`,
  `dotagent-plugin-preflight-warp`). **Notifiers built-in NÃO são plugins** —
  vivem em `dotagent-notify/src/drivers/`. Plugins `notify-*` só existem
  como escape hatch pra terceiros (Discord, Teams, etc) via
  `driver = "plugin"`.
- Bin do plugin tem o mesmo nome do pacote — convenção do PluginClient

## Self-verification (antes de reportar "done")

```bash
cargo fmt --all                              # formatar
cargo fmt --all -- --check                   # CI verifica isso
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

**Se qualquer um falhar, fixa antes de marcar como done.** A CI roda os três
em macOS + Linux — quebrar `-D warnings` é quebrar a build.

### Doc-review é obrigatório antes de "done"

Qualquer mudança user-visible (manifest schema, plugin protocol, novo
plugin/comando/env var, mudança de path, novo evento, etc.) tem
consequência em docs. **Sempre rode** [`dotagent:doc-review`](.claude/skills/doc-review/SKILL.md)
antes de fechar a tarefa. Drift de doc é dívida pesada — pega no MVP,
não daqui a 3 meses quando ninguém lembra mais do que mudou.

Casos típicos de drift:

- Mudei o nome de uma variável de ambiente injetada → `docs/reference/agent-spec.md` + `docs/concepts/agents.md` + cada plugin doc que mencione.
- Adicionei um plugin novo em `plugins/` → `docs/plugins/<name>.md` + 2 índices + Homebrew formula + Cargo workspace members.
- Mudei `~/.config/dotagent/state/...` → toda menção de path em todos os docs.
- Adicionei evento (`recovered`, `given_up`, ...) → tabela em `docs/concepts/plugins.md` + `docs/reference/plugin-protocol.md`.

A skill `dotagent:doc-review` automatiza essa busca e devolve a punch
list. Não pule.

Smoke E2E rápido (precisa do hello-fish):

```bash
cargo build --release -p dotagent
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent doctor
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish --schedule manual --dry-run
```

## Patterns que NÃO violar

### 0. Defaults sensatos out-of-the-box

**Toda feature deve funcionar sem o usuário escrever uma linha de
config.** Config files (`config.toml`, agent `[security]`, plugin
config) são pra **customizar**, não pra ativar funcionalidade básica.

- ✅ Logs estruturados, rotation, retention, sweeper → todos defaults
  sensatos. Usuário NÃO precisa criar `config.toml`.
- ✅ OTel export → desligado por default. Se quiser, basta 2 linhas no
  `config.toml`.
- ✅ Heartbeat path, audit log, plugin discovery → tudo defaults.

Quando adicionar config nova: campo obrigatório só se NÃO houver default
razoável (ex: `to = "+55..."` no notifier `imessage` — não há default
universal). Pra todo o resto, escolha um default sensato e documente
como **override** no doc.

Anti-pattern: `dotagent doctor` mostrar "you must create config.toml
before using X" — significa que o default foi mal escolhido ou está
faltando.

A doc segue o mesmo princípio: **começa com "out of the box"**, depois
"Customizing" como apêndice. Veja `docs/guides/observability.md` como
exemplo do padrão.

### 1. Heartbeat shape é compatível com Fish framework legacy

`crates/dotagent-core/src/heartbeat.rs::Heartbeat` foi modelado pra ler/escrever
o **mesmo arquivo** que o `lib/agent.fish` do dotfiles Avelino escreve. Se
você mudar o shape:

- Quebra a migração gradual (Fish e dotagent coexistem lendo o mesmo `.heartbeat.json`)
- Invalida histórico de execuções existente

Mudanças aditivas (novos campos opcionais) são ok. Renomear/remover campo
existente requer migração documentada.

### 2. Plugin protocol é estável

`info` / `validate` / `invoke` + JSON stdio é contrato público. Comunidade
escreverá plugins em Go/Python/Bash assumindo essa interface. Se mudar:

- Bump major do `dotagent-plugin` crate
- Atualizar `docs/reference/plugin-protocol.md`
- Anotar no CHANGELOG

### 3. Scheduler é puro

`dotagent-scheduler` **não importa `std::fs`, `std::env`, `chrono::Local::now()`**
fora de testes. Tudo recebe `now: DateTime<Local>` como argumento. Isso é o
que faz o teste de "weekday off" + "BSD-date edge cases" caber em milissegundos.

### 4. dotagent não substitui o `mcp` CLI

`mcp` é um proxy MCP separado (em Rust, já existe — `~/.cargo/bin/mcp`).
Agents continuam chamando `mcp` diretamente. dotagent NÃO importa `mcp`,
NÃO embute MCP, NÃO substitui MCP. Reaproveite quando o plugin precisar
(ex: `sink-roam` chama `mcp roam_publish ...`).

## Patterns que SEGUIR

### Adicionar plugin novo

Veja [`.claude/skills/new-plugin/SKILL.md`](.claude/skills/new-plugin/SKILL.md).
TL;DR: copie um plugin existente, troque o nome, registre no `Cargo.toml`
do workspace, implemente os 3 verbs.

### Adicionar subcomando à CLI

Veja [`.claude/skills/new-command/SKILL.md`](.claude/skills/new-command/SKILL.md).
TL;DR: enum `Command` em `main.rs` + função em `commands/mod.rs`.

### Migrar agent fish → dotagent

Veja `docs/guides/migrating-from-fish.md`. Resumo: cria `agent.toml`, mantém o
`agent.fish` original, `dotagent install`.

## Linguagem das mensagens

- **Commits e código**: inglês (kebab-case branches, conventional commits opcional)
- **Docs `.md`**: inglês (projeto open-source)
- **Comentários explicando "por que"**: inglês curto
- **Comunicação com Avelino na conversa**: pt-BR informal (estilo dele)

## Commits

Não rode `git commit`/`git push` automaticamente. Política global do
Avelino bloqueia automação de commit — só ele commita.

## Trade-offs já decididos (não reabra sem nova discussão)

- **Daemon único** (`run.avelino.dotagent`) em vez de 1 plist por agent
  schedule. Justificativa: scheduler adaptativo centraliza inteligência.
- **Subprocess + JSON** pra plugins (`preflight`, `sink`, notifier de terceiro
  via escape hatch), **não WASM, não dylib**. Justificativa: permite plugin
  em qualquer linguagem, crash do plugin não derruba daemon.
- **Notifiers built-in no daemon** (crate `dotagent-notify`), **não plugin
  subprocess**. Justificativa: notify é caminho quente (toda falha dispara),
  o fork + JSON marshal/unmarshal por notificação era caro e exigia binários
  extras no PATH (`osascript`, `notify-send`). APIs nativas (NSUserNotification,
  D-Bus, reqwest) são in-process, mais rápidas e sem dependência implícita
  do OS. Plugin protocol sobrevive como escape hatch (`driver = "plugin"`)
  pra Discord/Teams/etc.
- **TOML** pra manifest, **não YAML, não JSON**. Justificativa: comentários,
  tipos claros, ergonomia Rust-friendly.
- **Heartbeat shape clone do Fish framework**. Justificativa: migração
  gradual sem invalidar histórico.
- **`mcp` CLI continua separado**. Justificativa: separação de preocupações
  (orquestrador vs. proxy MCP).

Pra reabrir qualquer um: abra issue com nova justificativa, debata, atualize
este doc.
