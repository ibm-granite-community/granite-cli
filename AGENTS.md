## Project Practices

- Avoid adding new dependencies if an existing dependency or standard library could serve the same function
- Use the minimal visibility scoping for every function (only `pub` if necessary, crate-pub unless full pub necessary)
- When laying out a source file, organize it as follows:
    1. `use` directives
    2. Public functions/structs/enums/etc (mark with `/*-- public --*/` to begin section)
    3. Private functions/structs/enums/etc (mark with `/*-- private --*/` to begin seciton)
    4. Tests (mark with `/*-- tests --*/` to begin seciton)
- When formatting code, use `./scripts/fmt.sh` which runs both `clippy` and `fmt` with the right versions of the rust toolchain via `rustup`
- When writing unit tests, nothing should _ever_ depend on the local environment. This includes hardware detection, existence of pre-installed binaries, internet connectivity, and anything else that could differ between development environments

## End-to-end and manual testing

- The end-to-end test kit lives in `tests/e2e/`; `tests/e2e/README.md` describes it. Use and extend it (`lib.sh`, `fixtures/`, `run.sh`, `manual-test-template.md`) for new end-to-end tests, fixtures and manual test guides
- Run the CI set with `tests/e2e/run.sh`, and the tests that need a local environment with `tests/e2e/run.sh --local`
- A test in `tests/e2e/hermetic/` needs only the binary and the files it writes: no network, model server, hardware detection, launcher command on `PATH` or terminal. CI runs these. A test that needs any of those goes in `tests/e2e/local/` and skips itself when the requirement is missing
- When a feature changes behaviour that a script can check, add or update a hermetic test in the same branch. Assert on `-o json` output and do not depend on row order
- Write a feature branch's manual test guide from `tests/e2e/manual-test-template.md`, with setup through `tests/e2e/lib.sh`. Record the commit and machine the output was observed on, what was not verified, and findings
- In a manual test guide, a step a script can run is a test under `tests/e2e/` when it meets "Which kind of test to write" below, and a `[Scripted]` step kept in the guide only otherwise. Do not add a folder of tests that CI skips. Before the branch merges, turn a Scripted step into a test if it found a bug or reaches lines the unit tests miss. List each step's kind and status in the guide's test map, and tag step headings `[Automated]`, `[Scripted]` or `[Manual]`. End every command that shows scripted behaviour with `</dev/null`, so a pasted command does not prompt; give the keys and prompts for steps that need a terminal
- Add a fixture to `tests/e2e/fixtures/` when two or more tests or guides need it. Fixtures must load on `main`; a fixture for a branch-only feature lands with that feature
- The rule for unit tests above still applies: unit tests must not depend on the local environment. End-to-end tests are the place for behaviour that needs the built binary

### Which kind of test to write

- Prefer a unit test for logic: parsing, resolution, recommendations, compatibility rules, how a value is rendered. Give the code what crosses an external interface (`Ui`, `InstallEnv`, a `HardwareProfile`, a discovery result, `TestConfigHome`) instead of letting it reach the real one
- Write a hermetic test for what only runs in the built binary, or against the real filesystem, environment and processes:
    - command dispatch in `main.rs`: subcommand routing, the backend chosen by `-o`, exit codes, and what goes to stdout and what to stderr. Unit tests call the command functions directly and never run `main()`
    - config files on disk: fixtures in older or hand-written formats, files a command writes or removes, and read-only commands leaving every file unchanged
    - `GRANITE_CLI_HOME`, `HOME` and `PATH`, with `stub_bin` standing in for launchers and package managers
    - process lifecycle: exit status, signals, and child processes left running
- Write a local test for software granite-cli does not control and that can change in its own releases: model servers, agent tools launched through the proxy and its MCP servers, and the config files those tools read. Give hardware detection a smoke check only
- Leave the TUI, prompts answered with real keys, and macOS, Windows and WSL behaviour to the manual test guide until CI can run them
- Do not repeat unit-tested logic in an end-to-end test. Read state back through `-o json` instead of comparing granite-cli's own YAML files, and do not assert outcomes that depend on the machine, such as which models `setup --auto` picks
- The coverage job reports the unit, hermetic and local suites separately. Use it to check that a new end-to-end test reaches lines the unit tests miss
