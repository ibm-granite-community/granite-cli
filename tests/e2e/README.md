# End-to-end and manual testing

This directory holds the tools for testing the `granite-cli` binary from the
outside: a shell library, configuration fixtures, scripted tests, and a
template for the manual test guide of a feature branch. Unit tests stay in
`src/` as before.

```
tests/e2e/
├── README.md                    this guide
├── lib.sh                       helpers for scripts and for manual sessions
├── run.sh                       runs the scripted tests and reports the result
├── coverage.sh                  runs them against an instrumented build, writes LCOV
├── docker-compose.providers.yml Docker Compose config for AI model providers
├── manual-test-template.md      starting point for a feature's manual test guide
├── fixtures/                    configuration homes (GRANITE_CLI_HOME contents)
├── hermetic/                    tests that need only the binary
├── local/                       tests that need a local environment, such as Ollama
└── docker/                      custom Dockerfiles for providers without official images
```

Requirements: bash 3.2 or later, `jq`, and `curl` for the `local/` tests.
Docker and Docker Compose for running provider services locally.

CI runs both sets on Linux. The `e2e` job runs `hermetic/`. The `e2e-local`
job uses Docker Compose to start Ollama from `docker-compose.providers.yml`,
waits for it to be healthy, and runs `local/` with `--no-skip`, so a test
that would skip itself fails instead. The `check` workflow runs `shellcheck -x`
over every script here; `scripts/lint.sh` runs it too when `shellcheck` is
installed.

## Running the tests

```sh
tests/e2e/run.sh              # hermetic tests
tests/e2e/run.sh --local      # also the local/ tests; each skips itself
                              # when what it needs is missing
tests/e2e/run.sh --local --no-skip local/   # local/ only, skips count as
                              # failures (what the e2e-local CI job runs)
tests/e2e/run.sh -v remove    # one test, with its full output
GCLI_BIN=path/to/granite-cli tests/e2e/run.sh   # test another binary
```

`run.sh` builds `target/debug/granite-cli` once with the toolchain named by
`rust-version` in `Cargo.toml`, unless `GCLI_BIN` is set. Each test prints
numbered steps and one `ok` or `FAIL` line per check. `run.sh` shows a test's
output only when it fails, unless `-v` is given. `KEEP=1` keeps the scratch
directories so a failure can be inspected.

### Coverage

```bash
tests/e2e/coverage.sh lcov-e2e.info                  # hermetic tests
tests/e2e/coverage.sh lcov-e2e.info --local local/   # takes run.sh's arguments
```

`coverage.sh` builds `target/debug/granite-cli` with coverage
instrumentation through `cargo llvm-cov show-env`, runs `run.sh` against it
with the given arguments, and writes the line coverage of those runs as LCOV,
also when a test fails. It needs `cargo-llvm-cov` and the
`llvm-tools-preview` rustup component.

In CI the `e2e` and `e2e-local` jobs run their tests this way and upload the
LCOV files. The `coverage` job merges them with the unit test coverage
(`scripts/lcov.sh merge`), applies the minimums to the combined result, and
reports each suite and the combined coverage in the job summary and the pull
request comment. Its `coverage` artifact holds every LCOV file and an HTML
report of the combined line coverage.

## Hermetic and local tests

A test goes in `hermetic/` when its result depends only on the binary and
the files the test writes. It must not need network access, a model server,
a GPU or a particular amount of memory, a launcher command on `PATH`, or a
terminal.

A test goes in `local/` when it needs something from the machine: Ollama or
another server listening on its default port, an installed launcher, the
detected hardware, or an interactive terminal. It calls `require_url` or
`require_cmd` first, which exits with status 77 (reported as skipped) when
the requirement is missing. Its checks must hold for any machine that meets
the requirement: for example, after `setup --auto`, check that every model is
bound to a configured provider, and do not check which models were chosen.

The `e2e-local` CI job provides only Ollama on `localhost:11434`, with no
models pulled, and fails every test that skips. A new `local/` test that
needs anything else (a model, another server, a launcher, a terminal) needs
a matching step in that job in `.github/workflows/ci.yml`, or the job's
`run.sh` filter has to leave the test out.

Commands that are not hermetic on `main` today:

| Command | Depends on |
|---|---|
| `setup --auto`, `setup` | servers answering on `localhost:11434`, `:1234`, `:8080`; launcher commands on `PATH`; detected memory |
| `model recommend`, `hardware` | detected hardware |
| `provider health` against a running server | the server |
| `launch` without `--dry-run` | the launcher command, which takes over the terminal |
| any prompt (`* setup` wizards, `remove` without `--dependents` in a terminal) | an interactive terminal |

Two of these can be made hermetic: `provider health` against
`http://127.0.0.1:9` (nothing listens there, so it reports a failed
connection), and `launch --dry-run` with a stub launcher command
(`stub_bin opencode`).

## Using Docker Compose for local testing

The `docker-compose.providers.yml` file defines AI model provider services
that local tests can use. This approach provides:

- **Version matrix testing**: Change `OLLAMA_VERSION` env var to test different versions
- **Easy provider addition**: Add a new service to the compose file
- **Built-in health checks**: Services report when they're ready
- **Local dev/CI parity**: Same compose file works locally and in CI

### Starting providers locally

```sh
# Start Ollama and wait until it is healthy
docker compose -f tests/e2e/docker-compose.providers.yml up -d --wait ollama

# Check health status
docker compose -f tests/e2e/docker-compose.providers.yml ps

# View logs
docker compose -f tests/e2e/docker-compose.providers.yml logs ollama

# Stop and remove
docker compose -f tests/e2e/docker-compose.providers.yml down
```

### Adding a new provider

1. **If an official Docker image exists** (Ollama, LocalAI, vLLM):
   - Add a service to `docker-compose.providers.yml`
   - Include a health check that uses a command the image contains (the
     Ollama image has no `curl`, so its check runs `ollama list`)
   - Document the default port in comments

2. **If no official image exists** (LM Studio, llama.cpp):
   - Create a Dockerfile in `tests/e2e/docker/provider-name/`
   - Reference it in `docker-compose.providers.yml` with `build:` directive
   - Or use the existing binary installation approach in CI

3. **Update CI** (`.github/workflows/ci.yml`):
   - Add the provider to the `docker compose up -d` command
   - Tests automatically skip if the provider isn't running

### Testing with different provider versions

```sh
# Test with a specific Ollama version
OLLAMA_VERSION=0.39.0 docker compose -f tests/e2e/docker-compose.providers.yml up -d ollama

# In CI, use a matrix strategy:
# strategy:
#   matrix:
#     ollama_version: ["0.39.0", "0.40.0", "0.41.0"]
# env:
#   OLLAMA_VERSION: ${{ matrix.ollama_version }}
```

The image comes from Docker Hub unless `OLLAMA_IMAGE` names another
repository with the same tags, for example a mirror on a registry closer to
the runners:

```sh
OLLAMA_IMAGE=ghcr.io/OWNER/ollama docker compose -f tests/e2e/docker-compose.providers.yml up -d ollama
```

## Writing a hermetic test

Start from an existing test in `hermetic/`. A test is a bash script that
sources `lib.sh`, prepares a configuration, runs commands with `gcli`, checks
the result, and ends with `finish`:

```sh
#!/usr/bin/env bash
# One sentence: the behaviour this test covers.
# shellcheck source=tests/e2e/lib.sh
. "$(dirname "$0")/../lib.sh"

use_fixture basic

step "model list shows both models"
gcli model list -o json
expect_rc 0
expect_json 'ids | sort == ["granite-4.2-3b", "granite-4.2-8b"]'

finish
```

The helpers, all defined and described in `lib.sh`:

| Helper | Does |
|---|---|
| `fresh` | new empty `GRANITE_CLI_HOME`; sets `$P`, `$M`, `$C`, `$L` to its `providers/`, `models/`, `capabilities/`, `launchers/` |
| `fresh_home` | new empty `HOME`, for commands that may write launcher settings there |
| `use_fixture NAME` | `fresh`, then copy `fixtures/NAME` into it |
| `stub_bin NAME [CODE]` | put a stub command on `PATH` that records its arguments and exits with `CODE` |
| `step TEXT` | print a numbered step heading |
| `gcli ARGS...` | run the binary; stdout in `$OUT`, stderr in `$ERR`, exit status in `$RC` |
| `expect_rc N` | the exit status is `N` |
| `expect_json EXPR [TEXT]` | a `jq` expression over the JSON output is true |
| `json EXPR` | print a `jq` expression's value |
| `expect_out_contains`, `expect_err_contains` | a fixed string is in stdout or stderr |
| `expect_file`, `expect_no_file`, `expect_same_tree` | file and directory checks |
| `pass TEXT`, `fail TEXT` | record a check made by hand |
| `try ARGS...` | for manual guides: print the command, run it with `-o plain </dev/null`, print `exit N`; records no check |
| `require_url URL`, `require_cmd NAME`, `skip TEXT` | skip a `local/` test |
| `finish` | print the summary; exit 0 if every check passed |

`expect_json` reads the output of `-o json`, which is one JSON object per
line, as an array, and adds these `jq` functions: `table`, `ids`,
`col("HEADER")`, `row("id")` (the row as an object keyed by header),
`detail("title")` and `messages("info"|"warn"|"error")`.

Conventions:

- Assert on `-o json`. It has no colour codes and no column wrapping.
  Use `-o plain` in manual guides, where a person reads the output.
- Do not depend on row order. `provider list` and `provider catalog` rows are
  not sorted, and the catalog order changed between two runs here. Sort, or
  look rows up with `row("id")`.
- Every command runs with stdin from `/dev/null` and stderr to a file, so
  it never waits on a prompt, whatever the output format. A removal that
  would strand other instances then exits 1 unless `--dependents` is given,
  and the error names the three policies.
- `-o json` also carries log lines, as `{"type":"info"}` messages that
  contain `[KIND:WARN]`. `-l error` hides them.
- Check exit codes. A missing id exits 1; a list with broken references
  exits 0 and reports them in `NOTES` with a `⚠ ` prefix.
- A command that saves the configuration rewrites every file it loaded,
  in normalised form, and drops comments. Read-only commands leave the files
  as they are (`hermetic/config-listing.sh` checks this). `use_fixture`
  copies the fixture, so tests never change `fixtures/`.

## Fixtures

Each directory in `fixtures/` is the content of a `GRANITE_CLI_HOME`, with
one YAML file per instance, as the binary writes them. `fixtures/README.md`
lists what each one contains. Add a fixture when two or more tests or guides
need the same configuration; a one-off file can be written in the test with
`printf` into `$P`, `$M`, `$C` or `$L`.

Fixtures must load on `main`. A fixture for a feature that exists only on a
branch is added on that branch, together with the feature.

## Manual test guides

A feature branch's manual test guide describes what an end user can see,
how to set up a throwaway environment, and each step with its expected
output. Start from `manual-test-template.md` and use `lib.sh` in the setup
section; do not write new `fresh` and `fresh_home` helpers.

Guides are for reviewers and are usually kept with the pull request, not in
the repository. A guide is first-hand experience of a feature on one commit,
not a regression suite, so not every step that can run without a person
becomes a test:

- A step becomes a test in `hermetic/` or `local/`, in the same branch, when
  it meets "Which kind of test to write" in `AGENTS.md`: it covers command
  dispatch, files on disk, the environment, process lifecycle, or software
  granite-cli does not control. A reviewer runs it with `run.sh`, and CI
  keeps running it after the branch is merged.
- Any other scriptable step, typically one that repeats what unit tests
  already check, stays in the guide as a Scripted step: its commands and the
  output observed on the guide's commit. Nobody maintains it after the
  branch merges; it is a record of that commit.
- Before the branch merges, go through the Scripted steps once. A step that
  found a bug, or that reaches lines the unit tests miss in the coverage
  job's per-suite figures, becomes a test.

A test that nothing runs stops matching the CLI without anyone noticing, so
there is no folder for tests that CI skips.

A guide separates what a script runs from what a person runs:

- The guide starts with "How to run this plan": the `run.sh` command for the
  branch's tests, a test map, and a checklist of the manual steps.
- The test map has one row per step: what the step covers; its kind
  (Automated; Automated, needs a server; Scripted: guide only, with the
  reason it is not a test; Manual: needs a terminal; Manual: needs a person
  to judge something named); and its status on a named commit (Passed in
  the automated run, with the date; Observed, with the date; Run through a
  scripted pseudo-terminal, not typed by hand; Not run, with the reason;
  Failed, with the reason).
- Step headings carry `[Automated]`, `[Scripted]` or `[Manual: ...]`.
  Automated and Scripted steps keep their commands and expected output; an
  Automated step's are for investigating a failure of its test.

Interactive and non-interactive runs:

- A command can prompt when the output is `terminal` or `plain` and stdin is
  a terminal. `-o json` and `-o markdown` never prompt, and `</dev/null`
  makes any run non-interactive. A person who pastes a command into a
  terminal gets the prompt where a script got an error.
- Every command that shows scripted behaviour ends in `</dev/null`.
- A step that needs a terminal says so, and lists the keys to press and the
  prompts to expect, in order. A step with both parts is split in two.
- A prompt that pre-fills a default adds what the user types after the
  default. The guide tells the user to clear the field first.

Commands in a guide:

- Each command is complete (`-o plain`, `</dev/null`) and can be copied in
  one selection. Commands do not go in tables.
- For a batch of error cases, use `try` from `lib.sh`, followed by an
  "Expected:" block with the captured output. Outside `lib.sh`, the same
  function works in bash and zsh:

  ```sh
  try() { printf '$ granite-cli'; printf ' %q' "$@"; echo; "$B" "$@" -o plain </dev/null; echo "exit $?"; }
  ```

- Checks do not hard-code values that depend on the machine (memory,
  detected models, providers found by discovery).

What previous guides learned:

- State the commit and machine the output was observed on (CPU, memory,
  which servers were running). `setup --auto` picks different providers and
  models on different machines.
- Show the output a person sees (`-o plain`, or the terminal output when the
  layout is the point), and say what differs in the default terminal output
  (colours, wrapping).
- List what the change does not change, and what was read from the code but
  not run ("Not verified"). Readers cannot tell the two apart otherwise.
- Table fitting applies only when stdout is a terminal. To test it, run in an
  interactive terminal and set the width with `stty cols N`; do not pipe the
  output.
- Record findings in the guide as they come up, with the step that shows them.
