"""
Minimal example: Connect to agentsh MCP server and list available tools.

No API key needed -- this just connects to the MCP server and prints
the tools it exposes.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/list_tools.py
"""

import asyncio
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def main() -> None:
    if not AGENTSH_BIN.exists():
        print(f"Error: binary not found at {AGENTSH_BIN}")
        print("Build it first: cargo build --release")
        return

    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:
        tools = await server.list_tools()

        print(f"agentsh exposes {len(tools)} MCP tools:\n")
        for tool in tools:
            print(f"  {tool.name}")
            print(f"    {tool.description}")
            if tool.inputSchema and "properties" in tool.inputSchema:
                params = tool.inputSchema["properties"]
                required = tool.inputSchema.get("required", [])
                for name, schema in params.items():
                    req = " (required)" if name in required else ""
                    desc = schema.get("description", "")
                    print(f"      - {name}: {schema.get('type', '?')}{req}")
                    if desc:
                        print(f"        {desc}")
            print()


if __name__ == "__main__":
    asyncio.run(main())
