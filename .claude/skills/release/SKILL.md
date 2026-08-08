---
name: dotagent:release
description: Cut a new release of dotagent. Use when the user says "release X.Y.Z", "tag a release", "publish to crates.io". Handles version bump in workspace.package.version, CHANGELOG, git tag, and the cargo publish order across the workspace dependency graph.
---

# Skill: release

Corta release nova do dotagent.

## Quando usar

- "release 0.1.0"
- "tag o que tá no main"
- "publica nas crates.io"

## Pré-requisitos

- Tudo verde no `validate` skill
- `main` limpo (no uncommitted)
- Acesso a crates.io (`cargo login`) se for publicar

## Como aplicar

### 1. Decidir versão (semver)

- **patch (0.0.X)** — bugfix sem mudança de API
- **minor (0.X.0)** — feature nova, sem breaking
- **major (X.0.0)** — breaking change (manifest schema, plugin protocol, etc)

Enquanto pre-1.0: minor pode quebrar API. Avisar no CHANGELOG.

### 2. Bump da versão

**There are two places in the root `Cargo.toml`, not one.** Forgetting the
second makes `cargo clippy`/`test` fail immediately with `failed to select a
version for the requirement dotagent-core = "^0.3.0"` — internal crates
reference one another by version, and `^0.3.0` does not accept `0.4.0`.

```toml
[workspace.package]
version = "X.Y.Z"            # (1) o que cada crate herda

[workspace.dependencies]
# (2) TODAS as deps internas — o `version =` aqui é o que vai pro
# crates.io, o `path` só vale no workspace local
dotagent-core = { path = "crates/dotagent-core", version = "X.Y.Z" }
dotagent-scheduler = { path = "crates/dotagent-scheduler", version = "X.Y.Z" }
# ... e as outras dez
```

```bash
# Bump das duas de uma vez (ajuste OLD/NEW):
sed -i '' 's/version = "OLD"/version = "NEW"/' Cargo.toml
# Confere que não sobrou nenhuma:
grep -n 'version = "OLD"' Cargo.toml   # deve voltar vazio
```

Todas as crates do workspace herdam via `version.workspace = true`. Não
edita Cargo.toml individual.

### 3. CHANGELOG

Adiciona entrada no topo de `CHANGELOG.md`:

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Added
- ...

### Changed
- ...

### Fixed
- ...
```

### 4. Validar

```bash
# Skill validate completa
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### 5. Commit, depois tag

**Nesta ordem, e o agente não faz nenhum dos dois.** `cargo publish`
recusa working tree suja (`4 files ... not yet committed`), então o
commit não é opcional nem adiável — é pré-requisito do passo 6.

```bash
git add -A && git commit -m "release: X.Y.Z"
git tag -a vX.Y.Z -m "release X.Y.Z"
# NÃO push (commit policy do Avelino) — usuário faz commit/tag/push manual
```

Nunca passe `--allow-dirty` no publish pra contornar isso: publica
bytes que não existem em nenhum commit, e o que está no crates.io deixa
de ser reproduzível a partir da tag.

### 6. Publicar nas crates.io (ordem importa!)

A ordem é grafo de dependência: publicar primeiro o que ninguém depende.

> Atenção: `dotagent-core` **não** é nível 0. Ele depende de
> `dotagent-notify` (re-exporta `NotifierEntry` no manifest), que por sua
> vez depende de `dotagent-secrets`. Publicar core primeiro falha.

```bash
# Nível 0 — sem deps internas
cargo publish -p dotagent-secrets
cargo publish -p dotagent-supervisor
cargo publish -p dotagent-unit-gen
cargo publish -p dotagent-mcp
cargo publish -p dotagent-memory        # o CLI depende dele; esquecer aqui
                                        # só falha lá no nível 6

# Nível 1
cargo publish -p dotagent-notify        # → secrets

# Nível 2
cargo publish -p dotagent-core          # → notify

# Nível 3 — dependem do core
cargo publish -p dotagent-scheduler
cargo publish -p dotagent-state
cargo publish -p dotagent-plugin        # → supervisor

# Nível 4
cargo publish -p dotagent-telemetry     # → core, state

# Nível 5
cargo publish -p dotagent-runner        # → core, state, plugin, notify, supervisor, telemetry

# Nível 6 — CLI
cargo publish -p dotagent

# Plugins (independentes — só preflight e sink)
cargo publish -p dotagent-plugin-preflight-warp
cargo publish -p dotagent-plugin-preflight-cmd
cargo publish -p dotagent-plugin-sink-roam
cargo publish -p dotagent-plugin-sink-outl
cargo publish -p dotagent-plugin-sink-file
```

Pra reconferir o grafo depois de adicionar crate nova:

```bash
for c in crates/*/; do
  printf "%-22s -> %s\n" "$(basename "$c")" \
    "$(grep -oE '^dotagent-[a-z-]+\.workspace' "$c/Cargo.toml" | sed 's/\.workspace//' | tr '\n' ' ')"
done
```

Crates.io tem rate limit: deixa ~30s entre publishes.

### 7. GitHub release

```bash
gh release create vX.Y.Z --notes-from-tag
```

## Princípios

- **Versões sincronizadas no workspace.** Todas as crates do dotagent
  saem com a mesma versão pra evitar quebra de compatibilidade entre
  crates internas.
- **Plugins externos (community)** podem ter versão independente — não
  estão no workspace.
- **CHANGELOG é fonte da verdade do que mudou**. Tag message deve
  apontar pra ele.

## Anti-patterns

- ❌ Bump de versão sem CHANGELOG.
- ❌ Publicar `dotagent` antes das deps internas. `cargo publish` falha
  com erro de "X.Y.Z not found".
- ❌ Tag sem rodar `validate` antes. Tag aponta pra build quebrada =
  reverter no público.
