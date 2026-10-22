# Manual test: <feature>

<!--
Copy this file for a feature branch's manual test guide and replace every
<...>. Keep the section order. Delete the comments. See tests/e2e/README.md
for the conventions.
-->

Branch `<branch>`, tip `<short sha>`, on top of `origin/main` `<short sha>`.

| Commit | Content |
|---|---|
| `<sha>` | <one line per commit> |

All output below was observed on <machine: OS, CPU, memory> with the debug
build of `<sha>`, unless a section says otherwise. <Which servers were
running (Ollama on `localhost:11434`, ...) and which were not, when the
feature depends on discovery or hardware.>

## How to run this plan

<!--
A step that needs no person is either Automated or Scripted. It is
Automated, a test under tests/e2e/hermetic/ (or tests/e2e/local/ when it
needs a server, hardware or a launcher) added in this branch, when it meets
"Which kind of test to write" in AGENTS.md; that test keeps running after
the branch merges. Otherwise it is Scripted: its commands and the output
observed on this guide's commit stay in the guide only. The tests are what
a reviewer runs first; the Scripted and Manual steps are what is left.
-->

Run every automated step:

```sh
tests/e2e/run.sh --local <name filter>
```

It prints `PASS`, `FAIL` or `SKIP` for each script, and the checks of a
script that fails.

| Step | What it covers | Kind | Status on `<short sha>` |
|---|---|---|---|
| 1 | <behaviour> | Automated | Passed in the automated run (<date>) |
| 2 | <behaviour> | Automated, needs <Ollama on `localhost:11434`> | Not run: <reason> |
| 3 | <behaviour> | Scripted: guide only, <repeats unit tests of ...> | Observed on `<short sha>` (<date>) |
| 4 | <behaviour> | Manual: needs a terminal | Run through a scripted pseudo-terminal, not typed by hand |
| 5 | <behaviour> | Manual: needs a person to judge <the layout at 80 columns> | Failed: <reason, see Findings> |

Scripted and manual steps to run:

- [ ] Step 3: <one line>
- [ ] Step 4: <one line>
- [ ] Step 5: <one line>

## 1. What an end user can see

<!--
A numbered list of every user-visible change: commands, flags, columns,
messages, prompts, files written. Each item says what the user does and what
they see. Mark items read from the code and not run.
-->

1. **<Short statement of the change.>** <Details, with the exact text the
   user sees.>

What does **not** change:

- <Commands and screens a reader might expect to change, and that do not.>

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

A command can prompt when the output is `terminal` (the default) or `plain`
and stdin is a terminal. `-o json` and `-o markdown` never prompt. The
commands in `[Automated]` steps end in `</dev/null`, so they behave as in
the script when you paste them into a terminal.

## 3. Step-by-step test

### Step 1 [Automated]: <what this step shows>

<!--
An [Automated] step keeps its commands and expected output, for
investigating a failure of the script. Write each command complete, so it
can be copied in one selection. Do not put commands in tables. Do not
hard-code values that depend on the machine (memory, detected models) in
the expected output; mark them as such.
-->

```sh
use_fixture basic        # or: fresh, then write files with printf
printf 'provider_id: ...\n' > "$P/<id>.yaml"
$B <command> -o plain </dev/null
```

Expected:

```
<exact output; trim with "..." only where the text does not matter>
```

<What to look for in the output, and what differs with the default terminal
output (colours, wrapping).>

### Step 2 [Scripted]: <a batch of error cases>

`try` prints each command, runs it with `-o plain </dev/null` and prints the
exit status:

```sh
use_fixture basic
try model remove no-such-model
try <command> <args>
```

Expected:

```
$ granite-cli model remove no-such-model
<exact captured output>
exit 1
$ granite-cli <command> <args>
<exact captured output>
exit <n>
```

### Step 3 [Manual: needs a terminal]: <what this step shows>

<!--
A step that needs a terminal lists the keys to press and the prompts to
expect, in order. A step that has both scripted and interactive parts is
split into two steps.
-->

Run in an interactive terminal, without `</dev/null`:

```sh
use_fixture basic
$B <command>
```

1. At `<prompt text>`, press <keys>.
2. At `<prompt text>`, which shows `<default>`, clear the field first
   (the text you type is added after the default), then type `<value>` and
   press Enter.

Expected: <what the terminal shows after the last key>.

<!-- Repeat for each step. One behaviour per step. -->

## 4. Not verified

- <What was not run, and why: read from the code, needs other hardware, needs
  a server that was not running, needs the interactive wizard.>

## 5. Findings

<!--
One entry per problem found while testing: the step that shows it, what
happens, what was expected, and what goes wrong for the user as a result.
Write "None." when there are none.
-->

1. **<Short title>** (step <n>). <What happens, what was expected, and the
   consequence for the user.>
