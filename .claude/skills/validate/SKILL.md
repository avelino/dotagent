---
name: dotagent:validate
description: Run the full validation pipeline for dotagent (fmt, clippy with -D warnings, workspace tests, and end-to-end smoke test). Use before claiming a change is "done", before opening a PR, or when CI fails and the user wants to reproduce locally. Reports each step and stops at the first failure.
---

# Skill: validate

Pipeline de validação completa do dotagent. Roda em ordem e para na primeira
falha.

## Quando usar

- Antes de marcar mudança como done
- Antes de abrir PR
- Quando CI falha e quer reproduzir local
- Quando o user pergunta "tá tudo verde?"

## Passos (em ordem)

### 1. Formatter

```bash
cd $REPO_ROOT
cargo fmt --all -- --check
```

Se falhar:
```bash
cargo fmt --all  # aplica
```

### 2. Lint

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

CI é estrito (`-D warnings`). Warning = build broken.

### 3. Test

```bash
cargo test --workspace --all-targets
```

Cobertura esperada (baseline):
- `dotagent-scheduler`: ≥ 8 testes (expected_at, is_stale, should_retry, health_state, resolve_policy)
- `dotagent-state`: ≥ 2 testes (slug derivation, heartbeat roundtrip)

### 4. Smoke E2E

```bash
cargo build --release -p dotagent -p dotagent-plugin-preflight-warp
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent doctor
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish --schedule manual --dry-run
DOTAGENT_ROOT=$PWD/examples ./target/release/dotagent run hello-fish --schedule manual  # write heartbeat
cat ~/.local/state/dotagent/agents/hello-fish/default.heartbeat.json | jq '.exit_code'   # should be 0
```

`doctor` deve listar 4 manifests ok. `run` deve emitir env vars no stdout do
fish e exit 0. Heartbeat tem `exit_code: 0` e `last_success_at` preenchido.

### 5. Plugin smoke (info verb)

```bash
./target/release/dotagent-plugin-preflight-warp info | jq -e '.kinds == ["preflight"]'
```

(Notifiers built-in não respondem `info` — vivem dentro do daemon; smoke
deles é via `dotagent doctor` que lista `notifier driver=<x> (built-in)`.)

## Saída esperada

```
✓ cargo fmt — clean
✓ cargo clippy — 0 warnings
✓ cargo test — N passed, 0 failed
✓ doctor — 4 manifests ok, notifiers listed as built-in
✓ run hello-fish --dry-run — exit 0
✓ run hello-fish — heartbeat written, exit 0
✓ plugin info — preflight-warp respond
```

Se qualquer um falhar, **não reporte "done"** — fixa primeiro.

## Quando NÃO usar

- Mudanças que só tocam `docs/**/*.md` — só rode markdownlint
- Mudanças que só tocam exemplos sem afetar workspace — só `cargo check`
- Você só leu código, não escreveu nada
