"""
Example: killing processes and handling timeouts.

Demonstrates kill_command for terminating runaway processes and timeout
handling for commands that take too long.
No API key needed.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/kill_and_timeout.py
"""

import asyncio
import json
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def call(server, tool: str, args: dict) -> dict:
    result = await server.call_tool(tool, args)
    return json.loads(result.content[0].text)


async def call_raw(server, tool: str, args: dict) -> str:
    """Return raw text (for error messages that aren't JSON)."""
    result = await server.call_tool(tool, args)
    return result.content[0].text


async def main() -> None:
    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:

        # 1. Start a long-running process and kill it
        print("=" * 60)
        print("1. Start a long-running process and kill it")
        print("=" * 60)
        await call(server, "start_command", {
            "command": "sleep 999",
            "id": "runaway",
        })
        print("  Started 'sleep 999'")

        # Verify it's running
        status = await call(server, "get_status", {"id": "runaway"})
        print(f"  Status: {status['status']}")

        # Kill it
        kill_result = await call(server, "kill_command", {"id": "runaway"})
        print(f"  Kill result: killed={kill_result['killed']}")

        # Verify it's done
        status = await call(server, "get_status", {"id": "runaway"})
        print(f"  Status after kill: {status['status']}")
        print()

        # 2. Try to kill an already-completed process (should error)
        print("=" * 60)
        print("2. Try to kill an already-completed process")
        print("=" * 60)
        await call(server, "start_command", {
            "command": "echo done",
            "id": "quick-one",
        })
        await call(server, "wait_command", {"id": "quick-one"})
        error_msg = await call_raw(server, "kill_command", {"id": "quick-one"})
        print(f"  Response: {error_msg}")
        print()

        # 3. Timeout via run_command
        print("=" * 60)
        print("3. Timeout via run_command (2s timeout on sleep 60)")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "sleep 60",
            "timeout_seconds": 2,
        })
        print(f"  Timed out: {result['timed_out']}")
        print(f"  Exit code: {result['exit_code']}")
        print(f"  Duration:  {result['duration_seconds']:.1f}s (not 60!)")
        print()

        # 4. Timeout via start_command (process-level timeout)
        print("=" * 60)
        print("4. Process-level timeout (3s timeout on a 60s command)")
        print("=" * 60)
        await call(server, "start_command", {
            "command": "sleep 60",
            "id": "will-timeout",
            "timeout_seconds": 3,
        })
        print("  Started with 3s timeout")

        result = await call(server, "wait_command", {"id": "will-timeout"})
        print(f"  Timed out: {result['timed_out']}")
        print(f"  Duration:  {result['duration_seconds']:.1f}s")
        print()

        # 5. A process that outputs before timing out
        print("=" * 60)
        print("5. Process with output before timeout")
        print("=" * 60)
        result = await call(server, "run_command", {
            "command": "echo 'starting...'; sleep 0.5; echo 'halfway'; sleep 60",
            "timeout_seconds": 2,
        })
        print(f"  Timed out: {result['timed_out']}")
        print(f"  Captured output before timeout: {result['output_head']}")
        print()

        # 6. Verify cleanup -- list should show all completed/failed
        print("=" * 60)
        print("6. Final state of all commands")
        print("=" * 60)
        commands = await call(server, "list_commands", {})
        for cmd in commands:
            print(f"  [{cmd['status']:>10}] {cmd['id']}: "
                  f"{cmd['command'][:40]}, {cmd['runtime_seconds']:.1f}s")
        print()

        print("All kill/timeout examples completed successfully!")


if __name__ == "__main__":
    asyncio.run(main())
