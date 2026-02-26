"""
Demo: Using agentsh MCP server with the OpenAI Agents SDK.

This script shows how an LLM agent can use agentsh to run shell commands
efficiently -- blocking at the OS level instead of polling -- and get structured
results back.

Prerequisites:
  1. Build agentsh:    cargo build --release
  2. Set OPENAI_API_KEY in your environment
  3. Install deps:     pip install openai-agents
  4. Run:              python examples/demo_openai_agents.py

Reference: https://openai.github.io/openai-agents-python/mcp/
"""

import asyncio
import os
from pathlib import Path

from agents import Agent, Runner
from agents.mcp import MCPServerStdio

# Path to the agentsh binary (built with `cargo build --release`).
AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def main() -> None:
    if not AGENTSH_BIN.exists():
        print(f"Error: agentsh binary not found at {AGENTSH_BIN}")
        print("Build it first: cargo build --release")
        return

    if not os.environ.get("OPENAI_API_KEY"):
        print("Error: OPENAI_API_KEY environment variable is not set")
        return

    # Launch agentsh as a stdio MCP server.
    # The SDK spawns the process, keeps pipes open, and cleans up on exit.
    async with MCPServerStdio(
        name="agentsh",
        params={
            "command": str(AGENTSH_BIN),
            "args": [],
        },
        cache_tools_list=True,
    ) as server:
        # Create an agent that has access to agentsh's tools.
        agent = Agent(
            name="DevOps Assistant",
            instructions="""You are a helpful assistant that can run shell commands.

You have access to agentsh tools for running commands efficiently:
- Use `run_command` to execute a command and get the result immediately.
- Use `start_command` + `wait_command` for long-running commands.
- Use `get_status` to check on running commands.
- Use `kill_command` to stop a command.
- Use `list_commands` to see all tracked commands.

When the user asks you to do something that requires running commands,
use these tools. Always report the results clearly.""",
            mcp_servers=[server],
        )

        # --- Example 1: Simple command execution ---
        print("=" * 60)
        print("Example 1: Run a simple command")
        print("=" * 60)

        result = await Runner.run(
            agent,
            "What version of Python is installed? Run `python3 --version`.",
        )
        print(result.final_output)
        print()

        # --- Example 2: Command with structured output ---
        print("=" * 60)
        print("Example 2: List files and analyze output")
        print("=" * 60)

        result = await Runner.run(
            agent,
            "List the files in the current directory (use `ls -la`) and tell me "
            "how many files there are and which is the largest.",
        )
        print(result.final_output)
        print()

        # --- Example 3: Command that fails ---
        print("=" * 60)
        print("Example 3: Handle a failing command")
        print("=" * 60)

        result = await Runner.run(
            agent,
            "Try to list the contents of /nonexistent_directory and tell me what happened.",
        )
        print(result.final_output)
        print()

        # --- Example 4: Multi-step workflow ---
        print("=" * 60)
        print("Example 4: Multi-step workflow")
        print("=" * 60)

        result = await Runner.run(
            agent,
            "Create a temporary file in /tmp called agentsh_test.txt with the content "
            "'hello from agentsh', then read it back to verify, and finally delete it. "
            "Report what happened at each step.",
        )
        print(result.final_output)
        print()


if __name__ == "__main__":
    asyncio.run(main())
