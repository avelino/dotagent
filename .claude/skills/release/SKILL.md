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

Em `Cargo.toml` raiz:

```toml
[workspace.package]
version = "X.Y.Z"
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

### 5. Tag

```bash
git tag -a vX.Y.Z -m "release X.Y.Z"
# NÃO push (commit policy do Avelino) — usuário faz o push manual
```

### 6. Publicar nas crates.io (ordem importa!)

A ordem é grafo de dependência: publicar primeiro o que ninguém depende.

```bash
# Nível 0 — sem deps internas
cargo publish -p dotagent-core
cargo publish -p dotagent-plugin

# Nível 1 — dependem do core
cargo publish -p dotagent-notify
cargo publish -p dotagent-scheduler
cargo publish -p dotagent-state
cargo publish -p dotagent-unit-gen

# Nível 2 — dependem de state
cargo publish -p dotagent-runner

# Nível 3 — CLI
cargo publish -p dotagent

# Plugins (independentes — só preflight e sink agora)
cargo publish -p dotagent-plugin-preflight-warp
cargo publish -p dotagent-plugin-preflight-cmd
cargo publish -p dotagent-plugin-sink-roam
cargo publish -p dotagent-plugin-sink-file
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
