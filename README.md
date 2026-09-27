# Smartloop

Smartloop is a local AI orchestration framework for extracting information from your own sources and generating new content. It runs on your device.

Use the command line interface as a companion to the Studio app. It talks to the local service Studio runs, so you can manage projects, check on the agent, and chat with the assistant from the terminal — handy for scripting a repeatable setup.

<img width="884" height="687" alt="cli-run-latte-df56cbde4925c0e214a91ade5bac6a96" src="https://github.com/user-attachments/assets/469de052-e3e0-4118-bca8-4494976507b1" />

More at:
[docs.smartloop.ai]([https://](https://smartloop.ai/docs/intro/)

## Install

macOS and Linux

```sh
curl -fsSL https://smartloop.ai/install | sh
```

Windows (PowerShell):

```powershell
irm https://smartloop.ai/install.ps1 | iex
```

The binary goes to `$CARGO_HOME/bin` (`~/.cargo/bin`) when a Rust toolchain
already owns that directory, since it is on your `PATH` anyway; otherwise to
`~/.local/bin`. If the chosen directory is not on your `PATH`, the installer
appends an export line to the startup file for your shell — `~/.bashrc`,
`~/.zshrc`, or `fish_add_path` in `config.fish` — so a new terminal picks it up.
Re-running the installer will not add that line twice.

On Windows the binary goes to `%USERPROFILE%\.smartloop\bin` and the installer
sets your user `PATH` through the registry. Restart the shell to pick it up.

Set `SMARTLOOP_CLI_INSTALL_DIR` to install elsewhere, or `SMARTLOOP_CLI_VERSION`
to pin a specific release.

Prebuilt binaries are published for Linux (x86_64, aarch64 — statically linked
against musl), macOS (Apple Silicon and Intel) and Windows (x86_64).

### From source

Requires Rust (2024 edition):

```sh
cargo install --path .
```

## Usage

List projects:

```sh
smartloop project list
```

Output is rendered as a table showing each project's ID, name, and whether it is a system project.

Create a project from a blank template:

```sh
smartloop project create --name my-project
smartloop project create --name my-project --description "Research notes"
```

A blank project starts with no skills; the service seeds it with the workspace
defaults. Everything the project stores — skills, documents, its index — lives
under the service's own project directory, so there is no working directory to
choose.

Import a project from an archive produced by an earlier export:

```sh
smartloop project create --import my-project.zip
smartloop project create --import my-project.zip --name restored-project
```

`--name` is optional here — pass it to rename the imported project. The import
gets a fresh project ID, and MCP OAuth credentials are stripped from the
archive on the way in.

Delete a project:

```sh
smartloop project delete --id <project-id>
```

Check which endpoint the CLI uses, whether the local agent is running there,
which model it has loaded, and the per-project agents it has started:

```sh
smartloop agent status
```

```
Endpoint: http://localhost:38540
Status:   healthy
Model:    sl-mini (Q4_K_M, 32768 ctx, 769 MB)
Process:  pid 8738, 250 MB
+--------------+------+-------+-------+------+--------+
| Project      | PID  | Port  | Alive | Idle | Memory |
+--------------+------+-------+-------+------+--------+
| general_chat | 2668 | 62110 | true  | 94s  | 122 MB |
+--------------+------+-------+-------+------+--------+
```

`Endpoint` is the URL the CLI talks to (`SMARTLOOP_API_URL`, or the default).
The command exits with status 1 when the agent can't be reached there.

Start an interactive chat with the local agent:

```sh
smartloop run
```

Without `--project`, `run` lists your projects and asks which one to chat in;
press Enter to take the server's current project. With a single project, or
when stdin isn't a terminal, the current project is used without asking.

Pass an initial prompt to send immediately; the session then keeps reading
new prompts from stdin until EOF, `/quit`, `/exit`, `/q`, or `exit`:

```sh
smartloop run "what are some things to do in madrid spain?"
```

Options:

```sh
smartloop run --project <project-id>   # skip the project prompt
smartloop run --session <session-id>   # resume an existing session; a new one is created when omitted
```

The response streams token by token as it's generated. Progress from the
agent's tool calls (web search, document lookup, model selection, etc.) is
printed on stderr as `[step] message` lines, colored when the terminal
supports it, so it doesn't interleave with the streamed answer on stdout.

## Configuration

The CLI connects to the Smartloop API at `http://localhost:38540` by default.
Point it elsewhere with `SMARTLOOP_API_URL`:

```sh
SMARTLOOP_API_URL=http://localhost:9000 smartloop project list
```

## License

Released under the [MIT License](LICENSE).
