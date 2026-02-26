"""
Example: gather system information.

A practical example showing how an agent would gather system diagnostics
by running multiple commands and aggregating results.
No API key needed.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/system_info.py
"""

import asyncio
import json
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def call(server, tool: str, args: dict) -> dict:
    result = await server.call_tool(tool, args)
    return json.loads(result.content[0].text)


async def gather_info(server, label: str, command: str) -> str | None:
    """Run a command and return its first output line, or None on failure."""
    result = await call(server, "run_command", {
        "command": command,
        "timeout_seconds": 10,
    })
    if result["exit_code"] == 0 and result["output_head"]:
        return result["output_head"][0]
    return None


async def main() -> None:
    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:

        print("=" * 60)
        print("System Information Report")
        print("=" * 60)
        print()

        # Gather system info in parallel using start_command
        checks = {
            "os": "uname -s",
            "kernel": "uname -r",
            "arch": "uname -m",
            "hostname": "hostname",
            "user": "whoami",
            "shell": "echo $SHELL",
            "home": "echo $HOME",
            "uptime": "uptime",
            "cpu_cores": "sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo unknown",
            "memory": "sysctl -n hw.memsize 2>/dev/null || free -h 2>/dev/null | head -2 | tail -1",
        }

        # Start all commands in parallel
        for name, cmd in checks.items():
            await call(server, "start_command", {
                "command": cmd,
                "id": f"info-{name}",
                "timeout_seconds": 5,
            })

        # Wait for all and collect results
        info = {}
        for name in checks:
            result = await call(server, "wait_command", {"id": f"info-{name}"})
            if result["exit_code"] == 0 and result["output_head"]:
                info[name] = result["output_head"][0]
            else:
                info[name] = "N/A"

        # Format human-readable memory
        if info.get("memory", "N/A") != "N/A":
            try:
                mem_bytes = int(info["memory"])
                info["memory"] = f"{mem_bytes / (1024**3):.1f} GB"
            except ValueError:
                pass  # Keep raw value

        # Display
        print(f"  OS:        {info.get('os', 'N/A')}")
        print(f"  Kernel:    {info.get('kernel', 'N/A')}")
        print(f"  Arch:      {info.get('arch', 'N/A')}")
        print(f"  Hostname:  {info.get('hostname', 'N/A')}")
        print(f"  User:      {info.get('user', 'N/A')}")
        print(f"  Shell:     {info.get('shell', 'N/A')}")
        print(f"  Home:      {info.get('home', 'N/A')}")
        print(f"  CPU Cores: {info.get('cpu_cores', 'N/A')}")
        print(f"  Memory:    {info.get('memory', 'N/A')}")
        print(f"  Uptime:    {info.get('uptime', 'N/A')}")
        print()

        # Tool versions
        print("=" * 60)
        print("Development Tools")
        print("=" * 60)
        print()

        tools = {
            "python3": "python3 --version 2>&1",
            "node": "node --version 2>&1",
            "rustc": "rustc --version 2>&1",
            "cargo": "cargo --version 2>&1",
            "git": "git --version 2>&1",
            "docker": "docker --version 2>&1",
            "go": "go version 2>&1",
            "java": "java --version 2>&1 | head -1",
        }

        for name, cmd in tools.items():
            result = await call(server, "run_command", {
                "command": cmd,
                "timeout_seconds": 5,
            })
            if result["exit_code"] == 0 and result["output_head"]:
                version = result["output_head"][0]
                print(f"  {name:>10}: {version}")
            else:
                print(f"  {name:>10}: not installed")

        print()

        # Disk usage
        print("=" * 60)
        print("Disk Usage (top 5 by size)")
        print("=" * 60)
        print()
        result = await call(server, "run_command", {
            "command": "df -h 2>/dev/null | head -6",
        })
        for line in result["output_head"]:
            print(f"  {line}")
        print()

        print("System information report complete!")


if __name__ == "__main__":
    asyncio.run(main())
