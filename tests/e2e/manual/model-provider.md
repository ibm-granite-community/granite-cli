# Manual test: ModelProviderCapability

Branch `ModelProviderCapability`, tip `d09d7d6` (working tree changes on top
of it), on top of `origin/main` `d09d7d6`.

| Commit | Content |
|---|---|
| `<uncommitted>` | Add the `model-provider` capability and binding, wire it into `opencode` and `pi`, and omit `contextWindow` for models whose provider reported no context |

All output below was observed on an Android/Termux aarch64 machine
(Samsung SM-F971U, Rust 1.99) with the debug build of `d09d7d6` plus the
working tree changes. Real Pi 1.5.0 was on `PATH`. No model server was
running; dry runs need none.

## How to run this plan

```sh
tests/e2e/run.sh --local model-provider
tests/e2e/run.sh --local pi-model-config
```

| Step | What it covers | Kind | Status on the working tree |
|---|---|---|---|
| 1 | Both launchers write a proxy-pointed provider entry per configured provider from the `model-provider` fixture | Automated | Passed in the automated run (`hermetic/model-provider.sh`, 18 checks) |
| 2 | Real Pi accepts the generated `models.json`, including a discovered model with no context length | Automated, needs real `pi` on `PATH` (`tests/e2e/local/pi-model-config.sh`) | Passed on `pi` 1.5.0 (`local/pi-model-config.sh`, 7 checks) |
| 3 | `opencode` dry run shows the generated `opencode.json` without selecting a default model | Scripted: repeats the automated test's first half for a reader | Observed on the working tree |
| 4 | `pi` dry run shows the generated `models.json` provider and the placeholder key | Scripted: repeats the automated test's second half for a reader | Observed on the working tree |
| 5 | A real launch routes model calls through the session proxy and records usage | Manual: needs a launcher command and a reachable model server | Not run: no model server on this machine |
| 6 | `opencode` / `pi` model pickers list every configured model | Manual: needs the real launcher and a terminal | Not run: launchers other than Pi were stubs here |

Scripted and manual steps to run:

- [ ] Step 5: start a provider, launch `opencode` without `--dry-run`, ask for a completion, and check `granite-cli session show` usage.
- [ ] Step 6: run `opencode` and `pi` and confirm every configured model appears.

## 1. What an end user can see

1. **A new capability type `model-provider`.** `capability catalog` lists it
   as `Model Provider`, with dependency `Models: all configured`.
2. **`opencode` and `pi` accept it.** `launcher info opencode` and
   `launcher info pi` report `Model Provider` in their supported
   capabilities; no other launcher does yet.
3. **One generated provider per granite-cli provider.** `launch opencode
   --dry-run` prints an `opencode.json` whose `provider` block has one entry
   per configured provider (`ollama` in the fixture), with every configured
   chat model in `models`, `npm` set to the provider's chosen dialect SDK,
   and `options.baseURL` pointing at the session proxy. Two providers that
   speak the same dialect stay distinct, so the same model name under two
   providers is unambiguous.
4. **The same for Pi.** `launch pi --dry-run` prints one `models.json`
   provider per configured provider, with `api`, a proxy-pointed `baseUrl`,
   the placeholder `apiKey`, and one `models` entry per configured model.
   A provider that speaks several dialects is exposed under the first one
   the launcher can express, so its models are not repeated behind several
   APIs.
5. **Unknown context lengths are omitted.** Pi rejects a `models.json` whose
   `contextWindow` is 0 or null. A discovered model whose provider did not
   report a context length is written without that field, so Pi applies its
   own default (128K) instead of discarding every provider in the file.
6. **No default model.** The model-provider capability only populates the
   provider list. `opencode.json` has no top-level `model`, and `pi` gets no
   `--provider`/`--model`, so the user picks a model in the tool.

What does **not** change:

- `agent-model` still selects the default model when both capabilities are
  enabled. When both name the same provider, the two entries are merged:
  the `agent-model` options are kept and the `model-provider` models are
  added.
- Launchers other than `opencode` and `pi` do not accept `model-provider`;
  enabling it there is reported as an unsupported binding.
- Provider entries are regenerated on every launch, as before.

## 2. Setup for the test

```sh
cd <path to the worktree of this branch>
bash
. tests/e2e/lib.sh
```

`lib.sh` needs `jq` for the automated runs. The scripted commands below run
the binary directly and need no `jq`. The local `pi` check needs a real `pi`
on `PATH`.

## 3. Step-by-step test

### Step 1 [Automated]: dry runs of both launchers

```sh
tests/e2e/run.sh model-provider
```

Expected: `PASS hermetic/model-provider.sh`.

### Step 2 [Automated, needs real Pi]: Pi accepts the generated file

```sh
tests/e2e/run.sh --local pi-model-config
```

Expected: `PASS local/pi-model-config.sh`. The test launches `pi` through a
stub, then runs the real `pi --list-models` against the generated
`models.json`. The fixture's `discovered-unknown-context` model must appear
with Pi's 128K default and no schema error.

### Step 3 [Scripted]: `opencode` provider entries

```sh
use_fixture model-provider
fresh_home
stub_bin opencode
$B launch opencode --dry-run -o json </dev/null
```

Expected, inside the `info` message that is the generated config:

```json
"provider": {
  "ollama": {
    "npm": "@ai-sdk/openai-compatible",
    "name": "ollama",
    "options": { "baseURL": "http://127.0.0.1:<session-proxy>/v1" },
    "models": {
      "discovered-unknown-context": { "name": "..." },
      "hf.co/ibm-granite/granite-4.2-3b-GGUF:Q8_0": { "name": "..." },
      "hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0": { "name": "..." }
    }
  }
}
```

There is no top-level `"model"` key. The proxy URL is a placeholder because a
dry run starts no proxy.

### Step 4 [Scripted]: `pi` provider entries

```sh
use_fixture model-provider
fresh_home
stub_bin pi
$B launch pi --dry-run -o json </dev/null
```

Expected, one `info` message (Ollama speaks both dialects and the launcher
prefers OpenAI):

```json
{
  "api": "openai-completions",
  "apiKey": "$GRANITE_CLI_PI_API_KEY",
  "baseUrl": "http://127.0.0.1:<session-proxy>/v1",
  "models": [
    { "id": "discovered-unknown-context" },
    { "contextWindow": 131072, "id": "hf.co/ibm-granite/granite-4.2-3b-GGUF:Q8_0" },
    { "contextWindow": 131072, "id": "hf.co/ibm-granite/granite-4.2-8b-GGUF:Q8_0" }
  ]
}
```

The discovered model has no `contextWindow` key at all. The `args:` line is
`(none)`: Pi picks the model, not granite-cli.

### Step 5 [Manual: needs a model server]: routing and usage

Not run here. With Ollama serving on `localhost:11434` and a model pulled,
launch `opencode` without `--dry-run` from the `model-provider` fixture,
select one of the generated models, and ask a question. Confirm the request
reaches Ollama (not a direct URL in the tool's config) and that
`granite-cli session show` records usage for that model.

### Step 6 [Manual: needs a terminal]: model picker

Not run here. Run `opencode` and `pi` with the `model-provider` fixture and
open each tool's model picker. Every model from the fixture must be listed.

## 4. Not verified

- Real proxy routing and usage tracking (step 5): no model server was
  running.
- The real `opencode` model picker, and the real `pi` picker beyond
  `--list-models` (step 6): `opencode` was a stub here.
- An Anthropic-only provider: no provider type currently supports only
  Anthropic. Each provider's chosen dialect is unit-tested with a fake
  provider, but the fixture's Ollama provider is exposed under OpenAI, so
  the Anthropic branch is not exercised end to end.
- Provider custom headers are not included in a provider entry contributed
  only by `model-provider`; they are kept when that entry is merged into an
  `agent-model` entry for the same provider. This limitation predates the
  change and affects other proxied routes too.

## 5. Findings

1. **Pi rejected `contextWindow: 0` and ignored every provider in
   `models.json`** (steps 2 and 4). A discovered model can report no context
   length, and `granite-cli` rendered that as `0` (and `None` as `null`).
   Real Pi 1.5.0 reports `providers.<name>.models.N.contextWindow: must be >
   0` for both. The fix omits the field when the length is zero or unknown;
   real Pi then loads the file and applies its 128K default.
