---
name: dotagent:new-command
description: Add a new subcommand to the `dotagent` CLI. Use when the user asks to "add a command", "expose X as CLI", "create dotagent subcommand", or any extension to the CLI surface. Handles clap enum entry + commands/ implementation + help text consistency.
---

# Skill: new-command

Adiciona subcomando à CLI `dotagent`.

## Quando usar

- "adiciona `dotagent rotate-logs`"
- "expõe Y como comando"
- "queria um `dotagent inspect <agent>` que mostra o heartbeat"

## Como aplicar

### 1. Decidir o nome + escopo

| Padrão | Exemplo | Quando |
|---|---|---|
| Verbo simples | `tick`, `status`, `doctor` | Ação no orquestrador |
| Verbo + alvo | `install <agent>`, `run <agent>` | Ação sobre 1 agent |
| Sub-grupo | `plugin list`, `plugin invoke` | Família de subcomandos |

### 2. Adicionar variante no enum `Command`

Em `crates/dotagent/src/main.rs`:

```rust
#[derive(Subcommand, Debug)]
enum Command {
    // ... existing
    /// Short one-liner shown in `--help`.
    MyCommand {
        /// Description shown for this arg in `--help`.
        agent_name: String,
        #[arg(long)]
        flag: bool,
    },
}
```

### 3. Adicionar match arm no `main()`

```rust
match cli.command {
    // ... existing
    Command::MyCommand { agent_name, flag } => commands::my_command(agent_name, flag).await,
}
```

### 4. Implementar em `commands/mod.rs`

```rust
pub async fn my_command(agent_name: String, flag: bool) -> Result<()> {
    let agent = discovery::find_by_name(&agent_name)?;
    // ... lógica ...
    Ok(())
}
```

Se a função fica grande (>30 linhas), extrai pra `commands/my_command.rs` e
declara `mod my_command;` em `commands/mod.rs`.

### 5. Validar

```bash
cargo build -p dotagent
./target/debug/dotagent --help                  # verifica que aparece
./target/debug/dotagent my-command --help       # verifica help do command
./target/debug/dotagent my-command alguma-coisa # smoke test
```

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Princípios

- **Reutilize helpers**: `discovery::find_by_name`, `StateStore::from_home`,
  `PluginClient::from_environment`. Não duplique.
- **Não vaze detalhes de IO no enum**: argumentos `clap` são tipos primitivos
  (String, bool, Option<String>). A função em `commands/` resolve esses pra
  tipos do dominio.
- **Comportamento read-only** → não escreve heartbeat, não toca filesystem
  fora de `~/.local/state/dotagent`. Comandos de inspeção (`status`,
  `doctor`, `plugin list`) devem ser seguros pra rodar a qualquer hora.
- **Comportamento side-effectful** (`install`, `uninstall`, `run`, `tick`,
  `daemon`) → loga o que vai fazer + checa permissões.

## Anti-patterns

- ❌ Chamar `std::process::exit(N)` no meio da função — devolve `Result` ao
  `main` que decide o exit code. Exceção: `run` propaga exit do agent.
- ❌ Acessar `std::env::var` pra config que devia estar no `agent.toml`.
- ❌ Fazer parse de JSON/TOML manual em `commands/` — use `dotagent-core`.
