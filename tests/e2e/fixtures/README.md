# Fixtures

Each directory is the content of a `GRANITE_CLI_HOME`. Load one with
`use_fixture NAME` from `../lib.sh`, which copies it into a scratch
directory.

## basic

A complete, consistent configuration with no network dependency.

| Kind | Id | Notes |
|---|---|---|
| provider | `ollama` | `base_url: http://127.0.0.1:9`, where nothing listens, so health checks fail fast and nothing reaches a real server |
| model | `granite-4.2-3b`, `granite-4.2-8b` | both `GGUF/Q8_0` on `ollama` |
| capability | `agent-model` | uses `granite-4.2-8b` |
| capability | `sub-agent-explore` | uses `granite-4.2-3b` |
| launcher | `opencode` | enables both capabilities |

Every list command shows empty `NOTES`. `launch opencode --dry-run` needs an
`opencode` command on `PATH` (`stub_bin opencode`).

## broken-refs

Instances whose references cannot be resolved. Each list command reports
them in `NOTES` and exits 0.

| Kind | Id | Problem |
|---|---|---|
| model | `orphan` | `provider_id: gone`, not configured |
| model | `unknown-type` | `model_type: no-such-type`, not in the catalog |
| capability | `sub-agent-explore` | `model_id: missing-model`, not configured |
| launcher | `opencode` | enables `agent-model`, not configured |

## model-provider

A `model-provider` capability with three models behind one provider, and both
launchers that support it. `launch <launcher> --dry-run` writes one provider
entry named `ollama`, lists every model in it, and points it at the session
proxy. Both launchers need their command on `PATH` (`stub_bin opencode`,
`stub_bin pi`).

| Kind | Id | Notes |
|---|---|---|
| provider | `ollama` | `base_url: http://127.0.0.1:9`, where nothing listens |
| model | `granite-4.2-3b`, `granite-4.2-8b` | both `GGUF/Q8_0` on `ollama` |
| model | `discovered-unknown-context` | a discovered `custom` model with `context_length: 0`, so Pi must omit `contextWindow` |
| capability | `model-provider` | takes no settings; covers every configured model |
| launcher | `opencode`, `pi` | each enables `model-provider` |
