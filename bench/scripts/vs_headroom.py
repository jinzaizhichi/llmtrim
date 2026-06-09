#!/usr/bin/env python3
"""Head-to-head: llmtrim vs Headroom on Headroom's own content types.

Generates the content types from Headroom's published benchmark (JSON arrays, shell
output, build logs, grep results, source) and compresses each with **both** tools,
reporting tokens saved + latency side by side.

- llmtrim: always run (release binary; savings via its own bench, latency via the
  latency harness). Build first: `cargo build --release && cargo build --release --bench latency`.
- Headroom: run only if importable (`pip install "headroom-ai[all]"`). Skipped otherwise —
  fill from its published /docs/benchmarks table.

Usage: python3 bench/scripts/vs_headroom.py
"""
import json, subprocess, sys, time, tempfile, os, statistics
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def gen():
    """The six content types, sized to mirror Headroom's benchmark table."""
    def recs(n):
        a = []
        for i in range(n):
            r = {"ts": f"2026-04-{i%28+1:02d}T10:{i%60:02d}:00Z", "level": "INFO",
                 "service": f"svc-{i%5}", "msg": f"request {i} handled ok", "code": 200, "ms": i % 50}
            if i == 67:
                r.update({"level": "ERROR", "msg": "payment gateway declined",
                          "code": 402, "resolution": "retry with backup PSP", "affected": 1432})
            a.append(r)
        return json.dumps(a)
    return {
        "json_100": recs(100),
        "json_500": recs(500),
        "shell_200": "\n".join(f"drwxr-xr-x 2 u g {1024*i:8d} Apr {i%28+1:02d} file_{i}.txt" for i in range(200)),
        "buildlog_200": "\n".join(f"[{i:03d}] INFO compiling crate mod_{i} ok" for i in range(198))
                        + "\nERROR undefined reference to render_frame\nERROR build failed",
        "grep_150": "\n".join(f"src/{'abcde'[i%5]}.rs:{i+1}:    let v = connect({i});" for i in range(150)),
    }


def llmtrim_savings(name, content):
    """Run llmtrim's offline bench on one case → (tokens_in, tokens_out, pct)."""
    case = {"name": name, "request": json.dumps(
        {"model": "gpt-4o", "messages": [{"role": "user", "content": content}], "max_tokens": 300})}
    with tempfile.NamedTemporaryFile("w", suffix=".jsonl", delete=False) as f:
        f.write(json.dumps(case) + "\n"); corpus = f.name
    out = subprocess.run(
        ["cargo", "run", "-q", "--features", "live", "--", "bench",
         "--corpus", corpus, "--preset", "aggressive", "--offline"],
        cwd=ROOT, capture_output=True, text=True).stdout
    os.unlink(corpus)
    import re
    m = re.search(r"input (\d+) -> (\d+) tok \(([\d.]+)% saved\)", out)
    return (int(m[1]), int(m[2]), float(m[3])) if m else (0, 0, 0.0)


def headroom_savings(content):
    """Run Headroom if importable → pct saved, else None."""
    try:
        from headroom import compress  # type: ignore
        import tiktoken
        enc = tiktoken.get_encoding("o200k_base")
        msgs = [{"role": "user", "content": content}]
        t = time.perf_counter()
        out = compress(msgs)
        ms = (time.perf_counter() - t) * 1000
        before = len(enc.encode(content))
        after = len(enc.encode(out[0]["content"] if isinstance(out, list) else str(out)))
        return before, after, 100 * (1 - after / before), ms
    except Exception as e:
        return None


def main():
    rows = []
    for name, content in gen().items():
        lin, lout, lpct = llmtrim_savings(name, content)
        hr = headroom_savings(content)
        rows.append((name, lin, lpct, hr))
    print(f"{'content':14} {'tok':>7} {'llmtrim':>9} {'headroom':>10}")
    for name, lin, lpct, hr in rows:
        hr_s = f"{hr[2]:.0f}% {hr[3]:.0f}ms" if hr else "(not installed)"
        print(f"{name:14} {lin:>7} {lpct:>8.0f}% {hr_s:>10}")
    if not any(r[3] for r in rows):
        print("\nHeadroom not installed — compare against its published /docs/benchmarks table.")


if __name__ == "__main__":
    main()
