# agentsh

A shell for AI agents. Runs commands, blocks efficiently, returns structured output.

```
agent: run_command("npm install")  →  blocks  →  {
  "exit_code": 0,
  "duration_seconds": 14.2,
  "output_head": ["..."],
  "output_tail": ["added 1247 packages in 14s"],
  "total_lines": 89,
  "truncated": false
}
```

One tool call. Zero polling. Instant response when done.

## Why

AI agents today poll terminal output in a loop -- sleep, read, sleep, read -- wasting tokens and adding latency. agentsh blocks at the OS level and returns the instant the command finishes. Output is windowed (head + tail + error lines) so the agent gets the important parts without 500 lines flooding context.

## Install

### Homebrew (macOS/Linux)

```bash
brew install sanjay920/tap/agentsh
```

### Download binary

Grab the latest release from [GitHub Releases](https://github.com/sanjay920/agentsh/releases) and put it somewhere on your PATH.

### Build from source

```bash
git clone https://github.com/sanjay920/agentsh.git
cd agentsh
cargo build --release
# binary is at target/release/agentsh
```

## Setup

Add to your MCP config and restart.

### Cursor (`.cursor/mcp.json`)

```json
{
  "mcpServers": {
    "agentsh": {
      "command": "agentsh"
    }
  }
}
```

If you built from source instead of installing via brew/PATH:

```json
{
  "mcpServers": {
    "agentsh": {
      "command": "/path/to/agentsh/target/release/agentsh"
    }
  }
}
```

### Claude Code (CLI)

```bash
claude mcp add --transport stdio agentsh -- agentsh
```

If you built from source instead of installing via brew/PATH:

```bash
claude mcp add --transport stdio agentsh -- /path/to/agentsh/target/release/agentsh
```

Verify with `/mcp` inside Claude Code.

### Claude Desktop (`claude_desktop_config.json`)

```json
{
  "mcpServers": {
    "agentsh": {
      "command": "agentsh"
    }
  }
}
```

## Two modes

### Sessions (persistent, with PTY)

Sessions are persistent bash processes backed by a real pseudo-terminal. Working directory, env vars, shell functions, and aliases persist across commands. Programs that check `isatty()` (claude CLI, docker, ssh, colored output tools) work correctly.

```
create_session({ "id": "dev", "working_directory": "/my/project" })
session_exec({ "id": "dev", "command": "npm install" })
session_exec({ "id": "dev", "command": "npm test" })        # same cwd, same env
session_exec({ "id": "dev", "command": "export FOO=bar" })
session_exec({ "id": "dev", "command": "echo $FOO" })       # "bar" -- state persists
close_session({ "id": "dev" })
```

### Stateless (quick one-off commands)

Each `run_command` runs in a fresh `/bin/sh` -- no state between calls, no PTY. Faster for quick checks.

```
run_command({ "command": "git status", "working_directory": "/my/project" })
run_command({ "command": "which python3" })
```

For background execution: `start_command` + `wait_command` + `get_status` + `kill_command`.

## Output windowing

Commands that produce more output than `max_output_lines` are automatically windowed:

- **`output_head`** -- first 10 lines
- **`output_tail`** -- last N lines
- **`output_error_lines`** -- lines matching error/failure/panic patterns
- **`total_lines`** / **`truncated`** -- so the agent knows what it's missing

If the agent needs more, it calls `get_output` with the `id` and a line range.

## Security

- **Dangerous command blocking** -- `rm -rf /`, `mkfs`, `dd` to devices, fork bombs, `shutdown`, etc. are blocked before execution
- **Environment inheritance** -- full env inherited from the parent (like a real terminal). Opt-in stripping via `AGENTSH_STRIP_ENV`
- **Timeout ceiling** -- max 1 hour per command
- **Output buffer cap** -- 100k lines max to prevent OOM
- **Concurrent process limit** -- max 20 background processes
- **Process group isolation** -- each command runs in its own session for clean kill

## Development

```bash
cargo test           # 83 tests
cargo clippy         # lint
cargo fmt            # format
```

## License

MIT
