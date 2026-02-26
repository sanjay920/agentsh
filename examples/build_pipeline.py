"""
Example: practical build/test pipeline.

Simulates what an LLM agent would actually do: run a multi-step build pipeline
where each step depends on the previous one, check for errors, and report results.
No API key needed.

Usage:
  cargo build --release
  pip install openai-agents
  python examples/build_pipeline.py
"""

import asyncio
import json
import sys
from pathlib import Path

from agents.mcp import MCPServerStdio

AGENTSH_BIN = Path(__file__).parent.parent / "target" / "release" / "agentsh"


async def call(server, tool: str, args: dict) -> dict:
    result = await server.call_tool(tool, args)
    return json.loads(result.content[0].text)


async def run_step(server, name: str, command: str, **kwargs) -> dict:
    """Run a pipeline step and print status."""
    print(f"  [{name}] Running: {command}")
    result = await call(server, "run_command", {"command": command, **kwargs})
    status = "OK" if result["exit_code"] == 0 else "FAIL"
    print(f"  [{name}] {status} (exit={result['exit_code']}, "
          f"{result['duration_seconds']:.2f}s, {result['total_lines']} lines)")
    # Only show error lines if the command actually failed.
    if result["exit_code"] != 0 and result["output_error_lines"]:
        for err in result["output_error_lines"][:5]:
            print(f"  [{name}] ERROR: {err}")
    return result


async def main() -> None:
    async with MCPServerStdio(
        name="agentsh",
        params={"command": str(AGENTSH_BIN), "args": []},
    ) as server:

        print("=" * 60)
        print("Pipeline: Build and test agentsh itself")
        print("=" * 60)
        print()

        project_dir = str(Path(__file__).parent.parent)

        # Step 1: Check Rust toolchain
        result = await run_step(
            server, "TOOLCHAIN", "rustc --version && cargo --version",
            working_directory=project_dir,
        )
        if result["exit_code"] != 0:
            print("\n  Pipeline aborted: Rust toolchain not available")
            return
        for line in result["output_head"]:
            print(f"    {line}")
        print()

        # Step 2: Format check
        result = await run_step(
            server, "FMT CHECK", "cargo fmt -- --check",
            working_directory=project_dir,
        )
        if result["exit_code"] != 0:
            print("    Format issues found! Run: cargo fmt")
        else:
            print("    All files formatted correctly")
        print()

        # Step 3: Clippy lint
        result = await run_step(
            server, "CLIPPY", "cargo clippy -- -D warnings 2>&1",
            working_directory=project_dir,
        )
        if result["exit_code"] != 0:
            print("    Clippy warnings found!")
        else:
            print("    No clippy warnings")
        print()

        # Step 4: Run tests
        result = await run_step(
            server, "TEST", "cargo test 2>&1",
            working_directory=project_dir,
            max_output_lines=50,
        )
        # Extract test summary from output
        all_lines = result["output_head"] + result["output_tail"]
        test_results = [l for l in all_lines if "test result:" in l]
        for line in test_results:
            print(f"    {line.strip()}")
        print()

        # Step 5: Build release binary
        result = await run_step(
            server, "BUILD", "cargo build --release 2>&1",
            working_directory=project_dir,
        )
        if result["exit_code"] == 0:
            # Check binary size
            size_result = await call(server, "run_command", {
                "command": "ls -lh target/release/agentsh | awk '{print $5}'",
                "working_directory": project_dir,
            })
            size = size_result["output_head"][0] if size_result["output_head"] else "unknown"
            print(f"    Binary size: {size}")
        print()

        # Summary
        print("=" * 60)
        print("Pipeline Summary")
        print("=" * 60)
        commands = await call(server, "list_commands", {})
        all_passed = True
        for cmd in commands:
            icon = "PASS" if cmd["status"] == "completed" else "FAIL"
            if cmd["status"] != "completed":
                all_passed = False
            print(f"  [{icon}] {cmd['command'][:60]:<60} {cmd['runtime_seconds']:.1f}s")

        print()
        if all_passed:
            print("  All pipeline steps passed!")
        else:
            print("  Some steps failed. Check output above.")
            sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
