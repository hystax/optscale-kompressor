"""Replay an agent's actual tool output through the engine, one payload at a time.

Shared by the per-agent transcript readers. What it measures is what the
engine would do to each tool result on its own, not what the live proxy did:
the proxy also freezes cached prefixes and folds repeats across messages, so
the two numbers answer different questions and are printed under different
headings.
"""
import json
import subprocess
import tempfile
from pathlib import Path


def analyse(results, binary, config):
    """Print a per-payload table. `results` is a list of (label, content)."""
    print("\n  per-payload compression (local replay of actual tool output, not live savings)")
    print(f"  {'command / tool':<44}{'detected':<15}{'kind':<15}{'chars in':>10}{'out':>10}{'saved':>9}")
    total_in = total_out = 0
    with tempfile.TemporaryDirectory(prefix="sweep-payloads-") as directory:
        payload = Path(directory) / "payload.txt"
        for label, content in results:
            payload.write_text(content)
            outputs = []
            for command in ("detect", "compress"):
                args = [binary, command, "--config", config, "--json"]
                if command == "compress":
                    args += ["--mode", "recoverable"]
                run = subprocess.run([*args, str(payload)], capture_output=True, text=True)
                if run.returncode:
                    raise RuntimeError(f"{command} failed: {run.stderr}")
                outputs.append(json.loads(run.stdout))
            detected, compressed = outputs
            ci, co = compressed["chars_in"], compressed["chars_out"]
            total_in += ci
            total_out += co
            saved = 100 * (ci - co) / ci if ci else 0
            print(f"  {label[:43]:<44}{str(detected.get('content_type') or 'unknown'):<15}"
                  f"{str(compressed.get('kind') or 'unchanged'):<15}{ci:>10,}{co:>10,}{saved:>8.2f}%")
    print(f"\n  tool output chars {total_in:,} -> {total_out:,} "
          f"({100 * (total_in - total_out) / max(total_in, 1):.2f}% removed in local replay)")
