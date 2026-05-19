---
name: dotagent:new-plugin
description: Scaffold a new dotagent plugin (notify/preflight/sink). Use when the user asks to "add a plugin", "create a plugin for X", "support Pushover/Discord/Telegram/etc", or any new external integration. The skill produces a complete plugin crate that obeys the info/validate/invoke protocol and registers it in the workspace.
---

# Skill: new-plugin

Cria um plugin novo seguindo o protocolo `info`/`validate`/`invoke`.

## Quando usar

- "adiciona um plugin pra Discord" / "Telegram" / "Pushover"
- "quero notificar pra X em vez de iMessage"
- "preciso de um preflight que valide Y"
- "como envio output pro Notion?"

## Como aplicar

### 1. Decidir kind + nome

| Kind        | Quando                                         | Exemplo                |
|-------------|------------------------------------------------|------------------------|
| `notify`    | Notifier de terceiro (Discord, Teams, etc)     | `notify-discord`       |
| `preflight` | Checa pré-condição antes de rodar agent        | `preflight-warp`       |
| `sink`      | Persiste output do agent em algum lugar        | `sink-roam`            |

> **Heads up sobre `notify`**: os notifiers comuns (`desktop`, `slack`, `ntfy`,
> `pushover`, `imessage`) **NÃO são plugins** — vivem em `dotagent-notify/`
> como drivers built-in. Plugin `notify-*` só faz sentido pra integração de
> terceiro que não justifica entrar no daemon. Pra adicionar um driver
> built-in novo, edita `crates/dotagent-notify/src/` ao invés de criar plugin.

Nome do crate: `dotagent-plugin-<kind>-<nome>` (kebab-case).
Bin: mesmo nome.

### 2. Copiar do mais próximo

```bash
cp -r plugins/sink-file plugins/<kind>-<nome>
```

Plugin similar pra basear:
- HTTP API externa → `sink-roam` (POST a webhook MCP)
- Executa comando local → `preflight-cmd`
- Escreve em arquivo → `sink-file`

### 3. Editar `plugins/<kind>-<nome>/Cargo.toml`

```toml
[package]
name = "dotagent-plugin-<kind>-<nome>"
# ... resto idêntico ao copiado, ajustar description

[[bin]]
name = "dotagent-plugin-<kind>-<nome>"
path = "src/main.rs"
```

### 4. Implementar `src/main.rs` (3 verbs)

```rust
fn main() -> Result<()> {
    let verb = std::env::args().nth(1).ok_or_else(|| anyhow!("missing verb"))?;
    match verb.as_str() {
        "info"     => cmd_info(),
        "validate" => cmd_validate(),
        "invoke"   => cmd_invoke(),
        other => bail!("unknown verb: {other}"),
    }
}
```

#### `cmd_info` — descreve o plugin

```rust
fn cmd_info() -> Result<()> {
    let info = json!({
        "name": "<kind>-<nome>",
        "version": env!("CARGO_PKG_VERSION"),
        "kinds": ["<kind>"],
        "platforms": ["darwin", "linux"],   // ou só darwin/linux conforme aplicável
        "schema": {
            "type": "object",
            "required": ["<campo_obrigatorio>"],
            "properties": {
                "<campo>": { "type": "string" }
            }
        }
    });
    println!("{}", serde_json::to_string(&info)?);
    Ok(())
}
```

#### `cmd_validate` — checa config no startup

```rust
fn cmd_validate() -> Result<()> {
    let cfg: Config = serde_json::from_reader(std::io::stdin())?;
    let ok = !cfg.required_field.is_empty();
    println!("{}", serde_json::to_string(&Response {
        ok,
        error: if ok { None } else { Some("required_field missing") }
    })?);
    Ok(())
}
```

#### `cmd_invoke` — execução real

```rust
fn cmd_invoke() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let payload: InvokePayload = serde_json::from_str(&raw)?;
    // ... lógica ...
    println!("{}", serde_json::to_string(&Response { ok: true, error: None })?);
    Ok(())
}
```

### 5. Registrar no workspace

Edita `Cargo.toml` raiz, adiciona em `members`:

```toml
members = [
    # ...
    "plugins/<kind>-<nome>",
]
```

### 6. Smoke test

```bash
cargo build --release -p dotagent-plugin-<kind>-<nome>
./target/release/dotagent-plugin-<kind>-<nome> info | jq .
echo '{"campo":"valor"}' | ./target/release/dotagent-plugin-<kind>-<nome> validate
echo '{"kind":"<kind>","agent":"x","schedule":"y","event":"test","message":"hi","config":{"campo":"valor"}}' \
  | ./target/release/dotagent-plugin-<kind>-<nome> invoke
```

Os 3 comandos devem retornar JSON válido e exit 0 no caminho feliz.

### 7. Validar workspace

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Princípios

- **Single-purpose**: plugin faz UMA coisa. Auth refresh, retries, rate-limit
  HTTP são parte do plugin, não do daemon.
- **Stdout = JSON, stderr = log humano**: nada de mensagens diagnostic em
  stdout. Daemon parseia stdout como JSON estrito.
- **Exit 0 = sucesso na execução, !=0 = falha**. O daemon não retenta
  notificações (decisão arquitetural).
- **Platforms honestos**: se só funciona em macOS, declara `["darwin"]` no info.
  `doctor` usa essa info pra avisar.

## Anti-patterns

- ❌ Importar `dotagent-core` no plugin. Plugin é independente (poderia ser
  Python). Use `serde_json::Value` pra config opaca.
- ❌ Dar `panic!` em vez de retornar JSON de erro. `panic!` produz exit 101
  + stderr mas o daemon não consegue extrair `error: "..."`.
- ❌ Misturar lógica de notify e sink no mesmo plugin. Separar.
