# Fixtures

Each directory is the content of a `GRANITE_CLI_HOME`. Load one with
`use_fixture NAME` from `../lib.sh`, which copies it into a scratch
directory.

## basic

A complete, consistent configuration with no network dependency.

| Kind | Id | Notes |
|---|---|---|
| provider | `ollama` | `base_url: http://127.0.0.1:9`, where nothing listens, so health checks fail fast and nothing reaches a real server; `hardware: local` |
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

## hardware

Hardware profiles, and one provider linked to one of them. `local` is in manual mode with no
recorded detection, so commands use its figures and never compare them with
this machine: no result depends on the machine the test runs on.

| Id | Type | Memory |
|---|---|---|
| `local` | `local`, manual | system 32.0 GB |
| `box` | `custom` | discrete 24.0 GB |
| `spark` | `dgx-spark` (catalog) | 128.0 GB |

Provider `lab` (`llama-cpp`, `base_url: http://127.0.0.1:9`) runs on `box`.
