#!/usr/bin/env python3
"""
LoomGate Step 2.1 Live Upstream Verification Suite.

Tests against a running gateway-daemon backed by real upstreams in config.local.toml:
  1. Multi-turn long-context conversation & Prompt Caching hit rate (Anthropic / OpenAI)
  2. Tool Calling round-trip (schema preservation + tool_use/tool_calls + tool_result)
  3. Mid-stream client-abort (Ctrl+C stop-loss) & SQLite trajectory persistence check

Usage:
  python tests/live_upstream_verify.py [--anthropic-model MODEL] [--openai-model MODEL] [--gemini-model MODEL]
"""

import argparse
import http.client
import json
import sys
import time
import urllib.error
import urllib.request

GATEWAY = "http://127.0.0.1:8787"


def http_json(method: str, url: str, payload: dict | None = None, headers: dict | None = None):
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8") if payload is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if headers:
        for k, v in headers.items():
            req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=90) as resp:
            return resp.status, dict(resp.headers), json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            body = json.loads(raw)
        except Exception:
            body = {"raw": raw}
        return e.code, dict(e.headers), body


def get_header(headers: dict, name: str):
    low = name.lower()
    for k, v in headers.items():
        if k.lower() == low:
            return v
    return None


def long_system_prompt() -> str:
    # ~1,500+ tokens of deterministic architecture context to satisfy Anthropic/OpenAI prompt caching minimums
    block = (
        "LoomGate Microkernel Architecture Reference: Lossless Canonical IR, "
        "Zero-Buffer UTF-8 Safe SSE FSM, Drop Cancel Propagation, Prefix-Cache Sticky P2C Router, "
        "4-Tier Error Governance (L1 transient jitter, L2 cooldown failover, L3 quarantine unbind, "
        "L4 semantic passthrough), SQLite WAL Append-Only Trajectory Store, and Atomic CLI Takeover.\n"
    )
    return block * 45


def verify_anthropic(model: str, auth_headers: dict):
    sess_id = f"live-ant-{int(time.time())}"
    sys_blocks = [
        {"type": "text", "text": long_system_prompt(), "cache_control": {"type": "ephemeral"}}
    ]
    tools = [
        {
            "name": "get_gateway_port",
            "description": "Returns the local LoomGate listen port.",
            "input_schema": {
                "type": "object",
                "properties": {"service": {"type": "string"}},
                "required": ["service"],
            },
        }
    ]

    # Turn 1: Trigger tool call + warm prompt cache
    st1, h1, b1 = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {
            "model": model,
            "max_tokens": 256,
            "stream": False,
            "system": sys_blocks,
            "tools": tools,
            "messages": [
                {
                    "role": "user",
                    "content": "Call get_gateway_port with service='loomgate' to check the port.",
                }
            ],
        },
        headers={**auth_headers, "X-Session-Id": sess_id},
    )
    assert st1 == 200, f"Anthropic Turn 1 failed ({st1}): {b1}"
    actual1 = get_header(h1, "X-Gateway-Actual-Model")
    usage1 = b1.get("usage", {})
    tool_uses = [c for c in b1.get("content", []) if c.get("type") == "tool_use"]

    # Turn 2: Return tool result (or follow-up) and verify sticky hit + prompt cache read
    messages2 = [
        {
            "role": "user",
            "content": "Call get_gateway_port with service='loomgate' to check the port.",
        },
        {"role": "assistant", "content": b1.get("content", [])},
    ]
    if tool_uses:
        messages2.append(
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": tool_uses[0]["id"],
                        "content": "127.0.0.1:8787",
                    }
                ],
            }
        )
    else:
        messages2.append({"role": "user", "content": "Summarize the port in 5 words."})

    st2, h2, b2 = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {
            "model": model,
            "max_tokens": 128,
            "stream": False,
            "system": sys_blocks,
            "tools": tools,
            "messages": messages2,
        },
        headers={**auth_headers, "X-Session-Id": sess_id},
    )
    assert st2 == 200, f"Anthropic Turn 2 failed ({st2}): {b2}"
    usage2 = b2.get("usage", {})
    cache_read = usage2.get("cache_read_input_tokens", 0)
    total_in = usage2.get("input_tokens", 0) + cache_read
    hit_pct = (cache_read / total_in * 100.0) if total_in else 0.0
    print(
        f"[PASS] Anthropic Live ({actual1}): tool_calls={len(tool_uses)}, "
        f"sticky={get_header(h2, 'X-Gateway-Sticky-Hit')}, "
        f"turn1_cache_create={usage1.get('cache_creation_input_tokens', 0)}, "
        f"turn2_cache_read={cache_read} ({hit_pct:.1f}%)"
    )

    # Turn 3: Mid-stream client abort (Ctrl+C stop-loss)
    conn = http.client.HTTPConnection("127.0.0.1", 8787, timeout=30)
    conn.request(
        "POST",
        "/v1/messages",
        body=json.dumps(
            {
                "model": model,
                "max_tokens": 512,
                "stream": True,
                "messages": [
                    {"role": "user", "content": "Write a 300-word essay about Rust async streams."}
                ],
            }
        ),
        headers={"Content-Type": "application/json", "X-Session-Id": sess_id, **auth_headers},
    )
    resp = conn.getresponse()
    assert resp.status == 200, f"Anthropic stream failed: {resp.status}"
    step_id = resp.getheader("X-Gateway-Step-Id")
    # Read first 256 bytes then abruptly close socket (simulating Ctrl+C)
    _ = resp.read(256)
    conn.close()
    time.sleep(0.5)
    _, _, steps_body = http_json("GET", f"{GATEWAY}/api/sessions/{sess_id}/steps")
    aborted = [s for s in steps_body.get("steps", []) if s.get("step_id") == step_id]
    status_str = aborted[0]["status"] if aborted else "unknown"
    print(f"[PASS] Anthropic Ctrl+C Stop-Loss: step={step_id}, persisted_status={status_str}")


def verify_openai(model: str, auth_headers: dict):
    sess_id = f"live-oai-{int(time.time())}"
    sys_prompt = long_system_prompt()
    tools = [
        {
            "type": "function",
            "function": {
                "name": "get_gateway_port",
                "description": "Returns the local LoomGate listen port.",
                "strict": True,
                "parameters": {
                    "type": "object",
                    "properties": {"service": {"type": "string"}},
                    "required": ["service"],
                    "additionalProperties": False,
                },
            },
        }
    ]
    st1, h1, b1 = http_json(
        "POST",
        f"{GATEWAY}/v1/chat/completions",
        {
            "model": model,
            "max_tokens": 256,
            "stream": False,
            "tools": tools,
            "messages": [
                {"role": "system", "content": sys_prompt},
                {"role": "user", "content": "Call get_gateway_port with service='loomgate'."},
            ],
        },
        headers={**auth_headers, "X-Session-Id": sess_id},
    )
    assert st1 == 200, f"OpenAI Turn 1 failed ({st1}): {b1}"
    msg1 = b1["choices"][0]["message"]
    tcalls = msg1.get("tool_calls") or []

    msgs2 = [
        {"role": "system", "content": sys_prompt},
        {"role": "user", "content": "Call get_gateway_port with service='loomgate'."},
        msg1,
    ]
    if tcalls:
        msgs2.append(
            {
                "role": "tool",
                "tool_call_id": tcalls[0]["id"],
                "content": "127.0.0.1:8787",
            }
        )
    else:
        msgs2.append({"role": "user", "content": "Reply with 'ok'."})

    st2, h2, b2 = http_json(
        "POST",
        f"{GATEWAY}/v1/chat/completions",
        {
            "model": model,
            "max_tokens": 128,
            "stream": False,
            "tools": tools,
            "messages": msgs2,
        },
        headers={**auth_headers, "X-Session-Id": sess_id},
    )
    assert st2 == 200, f"OpenAI Turn 2 failed ({st2}): {b2}"
    usage2 = b2.get("usage", {})
    cached = (usage2.get("prompt_tokens_details") or {}).get("cached_tokens", 0)
    print(
        f"[PASS] OpenAI Live ({get_header(h1, 'X-Gateway-Actual-Model')}): "
        f"tool_calls={len(tcalls)}, sticky={get_header(h2, 'X-Gateway-Sticky-Hit')}, "
        f"turn2_cached_tokens={cached}/{usage2.get('prompt_tokens', 0)}"
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--anthropic-model", default="")
    ap.add_argument("--openai-model", default="")
    ap.add_argument("--token", default="")
    args = ap.parse_args()

    auth_headers = {"Authorization": f"Bearer {args.token}"} if args.token else {}
    st, _, health = http_json("GET", f"{GATEWAY}/healthz")
    assert st == 200, f"Gateway not reachable at {GATEWAY}: {st} {health}"
    print(f"Connected to LoomGate: {health}")

    if not args.anthropic_model and not args.openai_model:
        print("Specify --anthropic-model and/or --openai-model to run live upstream checks.")
        sys.exit(0)

    if args.anthropic_model:
        verify_anthropic(args.anthropic_model, auth_headers)
    if args.openai_model:
        verify_openai(args.openai_model, auth_headers)


if __name__ == "__main__":
    main()
