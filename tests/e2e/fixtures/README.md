# Fixtures

Each directory is the content of a `GRANITE_CLI_HOME`. Load one with
`use_fixture NAME` from `../lib.sh`, which copies it into a scratch
directory.

## basic

A complete, consistent configuration with no network dependency.

| Kind | Id | Notes |
|---|---|---|
| provider | `ollama` | `base_url: http://127.0.0.1:9`, where nothing listens, so health checks fail fast and nothing reaches a real server; `host: local` |
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

## hardware

Hardware profiles, a host that lists one of them, and a provider on that host. `local` is in manual mode with no
recorded detection, so commands use its figures and never compare them with
this machine: no result depends on the machine the test runs on.

| Id | Type | Memory |
|---|---|---|
| `local` | `local`, manual | system 32.0 GB |
| `box` | `custom` | discrete 24.0 GB |
| `spark` | `dgx-spark` (catalog) | 128.0 GB |

Host `box` (address `127.0.0.1`) lists the `box` profile, and provider `lab`
(`llama-cpp`, `base_url: http://127.0.0.1:9`) runs on host `box`.

## hosts

Hosts, the hardware profiles they list, and providers on them. Spec 0033
sub-task 1 adds this fixture with the acceptance tests, which skip until the
sub-task that makes them pass; it loads from sub-task 2 on. `local` is in
manual mode with no recorded detection, as in `hardware`, so no result
depends on the machine the test runs on. Every base URL uses its host's
address; nothing contacts them, and `ollama` uses port 9, where nothing
listens.

| Host | Address | Devices |
|---|---|---|
| `local` | `localhost` | `local` (manual, system 32.0 GB) |
| `pgx` | `pgx.lan` | `dgx-spark` (catalog, 128.0 GB) |
| `pgx2` | `pgx2.lan` | `dgx-spark`, shared with `pgx` |
| `box` | `box.lan` | `rtx-4090` twice (catalog, discrete 24.0 GB) |
| `lab` | `lab.lan` | none: hardware unknown |

| Provider | Type | Host | Notes |
|---|---|---|---|
| `ollama` | `ollama` | `local` | `http://localhost:9` |
| `llama-cpp-pgx` | `llama-cpp` | `pgx` | `http://pgx.lan:9797` |
| `llama-cpp-box` | `llama-cpp` | `box` | splits a model across devices by default |
| `openai-box` | `openai-compatible` | `box` | keeps the type's default: one device |
| `vllm-box` | `openai-compatible` | `box` | `splits_across_devices: true` |
| `vllm-lab` | `openai-compatible` | `lab` | fit not available |

## spec0027

A configuration written by spec 0027, before hosts: providers link to
hardware profiles with `hardware:`. Used to test the error such a file gives
and the conversion script.

| Provider | Base URL | `hardware:` |
|---|---|---|
| `ollama` | `http://localhost:9` | `local` |
| `openrouter` | `https://openrouter.ai/api` | `unspecified` |
| `ollama-spark`, `llama-cpp-spark` | `http://spark.lan:11434`, `http://spark.lan:9797` | `spark`: one profile, two providers at one address |
| `llama-cpp-card-a`, `llama-cpp-card-b`, `llama-cpp-card-v6` | `http://box-a.lan:8080`, `http://10.0.0.7:8080`, `http://[fe80::1]:8080` | `card`: one profile at three addresses, one of them IPv6 |

Hardware profiles: `local` (manual, system 32.0 GB), `spark` (`dgx-spark`,
128.0 GB) and `card` (`nvidia-rtx-4090`, 24.0 GB).
