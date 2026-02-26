"""
Example: run_command basics -- synchronous command execution.

Demonstrates the core tool: run a command, block until done, get structured output.
No API key needed.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/run_command_basics.py
"""

import asyncio
import json
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


def pretty(data: dict) -> str:
    return json.dumps(data, indent=2)


async def call(server, tool: str, args: dict) -> dict:
    """Call an agentsh tool and return the parsed JSON result."""
    result = await server.call_tool(tool, args)
    return json.loads(result.content[0].text)


async def main() -> None:
    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:

        # 1. Simple command
        print("=" * 60)
        print("1. Simple echo command")
        print("=" * 60)
        result = await call(server, "run_command", {"command": "echo 'hello world'"})
        print(f"  Exit code: {result['exit_code']}")
        print(f"  Output:    {result['output_head']}")
        print(f"  Duration:  {result['duration_seconds']:.3f}s")
        print()

        # 2. Multi-line output
        print("=" * 60)
        print("2. Multi-line output (seq 1 20)")
        print("=" * 60)
        result = await call(server, "run_command", {"command": "seq 1 20"})
        print(f"  Total lines: {result['total_lines']}")
        print(f"  Truncated:   {result['truncated']}")
        print(f"  First 3:     {result['output_head'][:3]}")
        print()

        # 3. Large output with windowing
        print("=" * 60)
        print("3. Large output windowed (seq 1 1000, max 30 lines)")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "seq 1 1000",
            "max_output_lines": 30,
        })
        print(f"  Total lines: {result['total_lines']}")
        print(f"  Truncated:   {result['truncated']}")
        print(f"  Head ({len(result['output_head'])} lines): {result['output_head'][:3]}...")
        print(f"  Tail ({len(result['output_tail'])} lines): ...{result['output_tail'][-3:]}")
        print()

        # 4. Command that fails
        print("=" * 60)
        print("4. Failing command (exit 42)")
        print("=" * 60)
        result = await call(server, "run_command", {"command": "exit 42"})
        print(f"  Exit code: {result['exit_code']}")
        print(f"  Timed out: {result['timed_out']}")
        print()

        # 5. Stderr capture
        print("=" * 60)
        print("5. Stderr capture")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "echo 'to stdout'; echo 'to stderr' >&2",
        })
        print(f"  Exit code: {result['exit_code']}")
        print(f"  Output (both streams): {result['output_head']}")
        print()

        # 6. Error pattern detection
        print("=" * 60)
        print("6. Error pattern detection")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": (
                "echo 'Starting build...';"
                "echo 'Compiling foo.rs';"
                "echo 'error[E0308]: mismatched types';"
                "echo 'Compiling bar.rs';"
                "echo 'warning: unused variable';"
                "echo 'error: aborting due to previous error';"
                "echo 'Build failed';"
            ),
        })
        print(f"  Total lines:  {result['total_lines']}")
        print(f"  Error lines:  {result['output_error_lines']}")
        print()

        # 7. Working directory
        print("=" * 60)
        print("7. Working directory (/tmp)")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "pwd",
            "working_directory": "/tmp",
        })
        print(f"  pwd output: {result['output_head']}")
        print()

        # 8. Timeout
        print("=" * 60)
        print("8. Timeout (sleep 30 with 1s timeout)")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "sleep 30",
            "timeout_seconds": 1,
        })
        print(f"  Exit code: {result['exit_code']}")
        print(f"  Timed out: {result['timed_out']}")
        print(f"  Duration:  {result['duration_seconds']:.1f}s (not 30!)")
        print()

        print("All run_command examples completed successfully!")


if __name__ == "__main__":
    asyncio.run(main())
