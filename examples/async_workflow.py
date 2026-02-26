"""
Example: async workflow -- start, monitor, and wait for commands.

Demonstrates the start_command + get_status + wait_command pattern for
running commands asynchronously while doing other work in between.
No API key needed.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/async_workflow.py
"""

import asyncio
import json
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def call(server, tool: str, args: dict) -> dict:
    result = await server.call_tool(tool, args)
    return json.loads(result.content[0].text)


async def main() -> None:
    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:

        # 1. Start a background command
        print("=" * 60)
        print("1. Start a background command")
        print("=" * 60)
        start_result = await call(server, "start_command", {
            "command": "sleep 2 && echo 'background task done'",
            "id": "bg-task-1",
        })
        print(f"  Started: id={start_result['id']}, status={start_result['status']}")
        print()

        # 2. Check status while it's running
        print("=" * 60)
        print("2. Check status (should be running)")
        print("=" * 60)
        status = await call(server, "get_status", {"id": "bg-task-1"})
        print(f"  Status: {status['status']}")
        print(f"  Runtime: {status['runtime_seconds']:.1f}s")
        print()

        # 3. Do other work in the meantime
        print("=" * 60)
        print("3. Do other work while bg-task-1 runs")
        print("=" * 60)
        other = await call(server, "run_command", {
            "command": "echo 'doing other work while background task runs'",
        })
        print(f"  Other work result: {other['output_head']}")
        print()

        # 4. Wait for the background command to finish
        print("=" * 60)
        print("4. Wait for bg-task-1 to complete")
        print("=" * 60)
        wait_result = await call(server, "wait_command", {"id": "bg-task-1"})
        print(f"  Exit code: {wait_result['exit_code']}")
        print(f"  Duration:  {wait_result['duration_seconds']:.1f}s")
        print(f"  Output:    {wait_result['output_head']}")
        print()

        # 5. Check status after completion
        print("=" * 60)
        print("5. Check status (should be completed)")
        print("=" * 60)
        status = await call(server, "get_status", {"id": "bg-task-1"})
        print(f"  Status: {status['status']}")
        print()

        # 6. Run multiple commands in parallel
        print("=" * 60)
        print("6. Run 3 commands in parallel")
        print("=" * 60)
        # Start 3 commands
        for i in range(1, 4):
            await call(server, "start_command", {
                "command": f"sleep {i} && echo 'task {i} done after {i}s'",
                "id": f"parallel-{i}",
            })
        print("  Started 3 parallel tasks")

        # List all commands
        commands = await call(server, "list_commands", {})
        running = [c for c in commands if c["status"] == "running"]
        print(f"  Running: {len(running)} commands")

        # Wait for all
        for i in range(1, 4):
            result = await call(server, "wait_command", {"id": f"parallel-{i}"})
            print(f"  parallel-{i}: exit={result['exit_code']}, "
                  f"duration={result['duration_seconds']:.1f}s, "
                  f"output={result['output_head']}")
        print()

        # 7. Final listing
        print("=" * 60)
        print("7. List all tracked commands")
        print("=" * 60)
        commands = await call(server, "list_commands", {})
        for cmd in commands:
            print(f"  [{cmd['status']:>10}] {cmd['id']}: {cmd['command'][:50]}")
        print()

        print("All async workflow examples completed successfully!")


if __name__ == "__main__":
    asyncio.run(main())
