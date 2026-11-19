# Manual test: hosts and setup (spec 0033)

Branch `specs/0033-hosts-and-setup-impl`, tip `<short sha>`, on top of
`origin/main` `<short sha>`.

| Commit | Content |
|---|---|
| `<sha>` | <one line per commit> |

All output below was observed on <machine: OS, CPU, GPUs, memory> with the
debug build of `<sha>`, unless a section says otherwise. <Which servers were
running on this machine and on the remote host.>

Steps 6 to 8 need hardware a CI runner does not have: a machine with two
NVIDIA or AMD GPUs, and a second machine on the network that runs a model
server. Steps 9 and 10 need a terminal.

## How to run this plan

Run every automated step:

```sh
tests/e2e/run.sh --no-skip upgrade-0027
tests/e2e/run.sh --no-skip hosts-fit
tests/e2e/run.sh --no-skip host-commands
tests/e2e/run.sh --no-skip setup-summary
tests/e2e/run.sh --local --no-skip setup-auto-ollama
```

It prints `PASS`, `FAIL` or `SKIP` for each script, and the checks of a
script that fails. The wizard's acceptance tests are in-process; run them
with `cargo test wizard_`.

| Step | What it covers | Kind | Status on `<short sha>` |
|---|---|---|---|
| 1 | A spec 0027 configuration fails to load, and the script converts it | Automated (`hermetic/upgrade-0027.sh`) | Not run yet |
| 2 | `model recommend --host` on a host with two cards | Automated (`hermetic/hosts-fit.sh`) | Not run yet |
| 3 | `host` commands: setup, list, info, remove; address change | Automated (`hermetic/host-commands.sh`) | Not run yet |
| 4 | `setup --auto` names a launcher left without a capability | Automated (`hermetic/setup-summary.sh`) | Not run yet |
| 5 | `setup --auto` writes the `local` host and links Ollama to it | Automated, needs Ollama on `localhost:11434` (`local/setup-auto-ollama.sh`) | Not run yet |
| 6 | Detection lists every GPU of this machine as a `local` device | Manual: needs a machine with two GPUs | Not run yet |
| 7 | Fit on two local GPUs for llama-cpp and Ollama | Manual: needs a machine with two GPUs | Not run yet |
| 8 | A remote host added from the command line and probed by the wizard | Manual: needs a second machine running a model server, and a terminal | Not run yet |
| 9 | The wizard with `local` and a remote host: ranking, ids, summary | Manual: needs a terminal and the remote host of step 8 | Not run yet |
| 10 | The TUI reads providers through their host | Manual: needs a terminal and a person to judge the Recommend section | Not run yet |

Scripted and manual steps to run:

- [ ] Step 6: on a machine with two GPUs, run `host info local` from a fresh configuration.
- [ ] Step 7: `model recommend --host local` with llama-cpp and Ollama configured.
- [ ] Step 8: `host setup` for the remote machine, then `host list` and `host info`.
- [ ] Step 9: `granite-cli setup` with both hosts checked, a provider on a non-default port, and quit once before the end.
- [ ] Step 10: open the TUI and check the Recommend section for a provider on each host.

## 1. What an end user can see

1. **A `host` command group.** `host list`, `host info <id>`, `host setup
   [<id>] [--address A] [--hardware H]...` and `host remove <id>`. `host
   list` shows `ID`, `ADDRESS`, `DEVICES` and `PROVIDERS`.
2. **Providers link to a host.** A provider file has `host:` in place of
   `hardware:`. `provider list` has a `HOST` column, and notes a base URL
   whose host part is not the host's address.
3. **A spec 0027 provider file does not load.** The error names the file and
   the conversion script in the release notes, `scripts/convert-0027-hosts.sh`.
4. **`hardware list` has a `HOSTS` column**, and `hardware remove` lists the
   hosts that use a profile.
5. **`model recommend --host <id>`** estimates fit on that host's devices,
   on all of them for a provider that splits a model across devices.
6. **Detection lists every GPU.** On a machine with two GPUs, the `local`
   host has the devices `local` and `local-2`.
7. **The setup wizard asks for hosts.** After the launchers it lists
   `local`, the configured hosts and the hosted services, checked, and "Add
   a host…". It probes each host, asks whether to add a provider at another
   address, and ranks models on every selected host before the plan. It
   writes nothing until the end.
8. **The summary reports what is missing.** A launcher with no capability is
   named with the command to bind one, and a host whose bound models need
   more memory together than it has is named.

What does **not** change:

- `hardware setup` and `hardware info` for a profile no host shares.
- `model recommend --hardware` still estimates on one device.
- `setup --auto` does not probe remote hosts.
- Sync, launch and the session proxy read a provider's `base_url` as before.

## 2. Setup for the test

Every command uses a throwaway `GRANITE_CLI_HOME`, and every `setup` or
`launch` run also a throwaway `HOME`, so your own configuration and launcher
settings are not read or written.

```sh
cd <path to the worktree of this branch>
bash                    # lib.sh needs bash; zsh cannot source it
. tests/e2e/lib.sh      # builds target/debug/granite-cli if missing; sets $B
```

`. tests/e2e/lib.sh` defines `fresh` (new empty `GRANITE_CLI_HOME`, with
`$P`, `$M`, `$C`, `$L` set to its `providers/`, `models/`, `capabilities/`
and `launchers/`), `fresh_home` (new empty `HOME`), `use_fixture NAME`
(`fresh` plus a copy of `tests/e2e/fixtures/NAME`) and `try` (runs one
command without prompts and prints its exit status). The scratch directories
are removed when the shell exits; start the shell with `KEEP=1` to keep them.
Run commands as `$B ...` to see their output.

Set `REMOTE` to the address of the second machine, and start a model server
on it (for example `llama-server --port 9797` or Ollama on its default port):

```sh
REMOTE=<host name or IP address of the second machine>
```

## 3. Step-by-step test

### Step 1 [Automated]: a spec 0027 configuration is converted

```sh
use_fixture spec0027
$B provider list -o plain </dev/null
scripts/convert-0027-hosts.sh "$GRANITE_CLI_HOME"
$B host list -o plain </dev/null
$B provider list -o plain </dev/null
```

Expected: the first command exits 1 with an error that names a provider
file, says `'hardware' was replaced by 'host'`, and links the release notes.
The script prints the hosts it wrote (`spark`, `card`, `card-2`) and the
providers it rewrote. `host list` shows the three hosts and `local`, and
`provider list` shows each provider with its host and no notes.

### Step 2 [Automated]: fit on two cards in a remote box

```sh
use_fixture hosts
$B model recommend --host box -o plain </dev/null
```

Expected: rows for `llama-cpp-box` and `vllm-box` have a FIT that ends in
`on box (2 × 24 GB)`; rows for `openai-box` end in `on box (24 GB)`.

### Step 3 [Automated]: host commands

```sh
use_fixture hosts
$B host list -o plain </dev/null
$B host info box -o plain </dev/null
$B hardware list -o plain </dev/null
$B host setup pgx --address 10.0.0.12 -o plain </dev/null
$B provider list -o plain </dev/null
```

Expected: `host list` shows `box` with `2 × rtx-4090` and `lab` with
`unknown`. `hardware list` shows `pgx, pgx2` in the HOSTS column of
`dgx-spark`. After the address change, which cannot prompt here, the base
URL of `llama-cpp-pgx` still uses `pgx.lan` and `provider list` notes it.

### Step 4 [Automated]: a launcher left without a capability

```sh
fresh; fresh_home; stub_bin opencode
GRANITE_CLI_TEST_DEFAULT_BASE_URL=http://127.0.0.1:9 $B setup --auto --skip-pull -o plain </dev/null
```

Expected: the summary ends with `⚠ Launcher 'opencode' has no capability`
and the `granite-cli capability setup` command.

### Step 5 [Automated]: `setup --auto` writes the `local` host

Needs Ollama on `localhost:11434`. Sub-task 6 adds these checks to
`local/setup-auto-ollama.sh`.

```sh
fresh; fresh_home; stub_bin opencode
$B setup --auto --skip-pull -o plain </dev/null
cat "$GRANITE_CLI_HOME/hosts/local.yaml" "$P/ollama.yaml"
```

Expected: `hosts/local.yaml` lists the `local` hardware profile, and
`providers/ollama.yaml` has `host: local`.

### Step 6 [Manual: needs a machine with two GPUs]: detection lists every GPU

```sh
fresh
$B host info local -o plain </dev/null
$B hardware list -o plain </dev/null
```

Expected: `Devices:` lists `local` and `local-2`, one per GPU, each with the
memory detection read. `hardware list` has both profiles, with `local` in
the HOSTS column. Record the GPUs and their memory here.

### Step 7 [Manual: needs a machine with two GPUs]: fit on both local GPUs

Start llama-server and Ollama on their default ports, then:

```sh
fresh; fresh_home
$B setup --auto --skip-pull -o plain </dev/null
$B model recommend --host local -o plain </dev/null
```

Expected: the FIT column for `llama-cpp` and `ollama` rows reads `on local
(2 × <memory> GB)`, and a model that needs more than one card's memory is
listed as fitting. Load one such model with llama-server and record whether
it loads, and the memory each GPU uses (`nvidia-smi`), next to the estimate.
This is the measurement Open Question 1 of the spec asks for.

### Step 8 [Manual: needs a second machine]: a remote host from the command line

Run in an interactive terminal, without `</dev/null`:

```sh
fresh
$B host setup
```

1. At `Host name or IP address:`, type `$REMOTE` and press Enter.
2. At `Hardware on <address>:`, pick `create new profile…`, then the family
   and card of the remote machine, and accept the profile id.
3. At `How many of these on <address>?`, type the number of cards.
4. At `Host id:`, accept the default.

Then:

```sh
$B host list -o plain </dev/null
$B host info <id> -o plain </dev/null
```

Expected: the host is listed with its address and devices, and no providers.

### Step 9 [Manual: needs a terminal and the host of step 8]: the wizard over two hosts

Start a model server on this machine at its default port, and llama-server on
the remote host at port 9797. Run in an interactive terminal:

```sh
fresh_home
$B setup
```

1. At the launchers question, keep the defaults.
2. At `Hosts to run models on`, keep `local`, the remote host and the hosted
   services checked, and press Enter.
3. The wizard prints `Looking for providers on localhost...` and then on the
   remote address. For the remote host, at `Add a provider on <address> at
   another address?`, pick `llama-cpp`, clear the field, type
   `http://<address>:9797` and press Enter. Then pick `No, continue`.
4. Pick capabilities and models. The model list marks catalog models a
   server has with `already on: <provider>`.
5. At the providers question, the provider on the remote host is listed as
   `llama-cpp-<host id>`.
6. Finish the run.

Expected: the summary lists both hosts, the provider ids, and a memory line
for a host whose bound models need more memory than it has, if any. Check
`$B provider list -o plain </dev/null`: the remote provider has `host:
<host id>` and its base URL uses the remote address.

Run `$B setup` again and quit with Ctrl-C at the variants question. Expected:
`find "$GRANITE_CLI_HOME" -newer "$GRANITE_CLI_HOME/hosts" -type f` lists
nothing written by the second run.

### Step 10 [Manual: needs a person to judge the TUI]: the TUI

```sh
$B tui
```

Expected: the Recommend section shows fit for a provider on each host; for a
host with several devices, the memory the provider type can use there.

## 4. Not verified

- <What was not run, and why.>

## 5. Findings

None yet.
