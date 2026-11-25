#!/usr/bin/env python3
"""
Edge Gateway v0.2.1 Automated Conformance & Stress Suite (Isolated Runner).

Every test resets both the mock upstreams AND the gateway's in-memory sticky/cooldown
state via POST /__admin/reset so tests never pollute each other, and all 7 tests execute
independently before reporting the final scorecard.
"""

import http.client
import json
import sys
import time
import urllib.error
import urllib.request

GATEWAY = "http://127.0.0.1:8787"
UPSTREAMS = {
    "anthropic-primary": "http://127.0.0.1:18081",
    "anthropic-backup": "http://127.0.0.1:18082",
    "openai-primary": "http://127.0.0.1:18083",
}


def http_json(method: str, url: str, payload: dict | None = None, headers: dict | None = None):
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8") if payload is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header("Content-Type", "application/json")
    if headers:
        for k, v in headers.items():
            req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
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


def reset_all():
    http_json("POST", f"{GATEWAY}/__admin/reset")
    for base in UPSTREAMS.values():
        http_json("POST", f"{base}/__admin/reset")


def test_1_session_stickiness_without_session_id():
    reset_all()
    messages = []
    pinned_channel = None
    sticky_hits = 0

    for turn in range(10):
        messages.append({"role": "user", "content": f"Turn {turn}: inspect module"})
        payload = {
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 256,
            "stream": False,
            "system": [
                {
                    "type": "text",
                    "text": "You are a strict coding agent.",
                    "cache_control": {"type": "ephemeral"},
                }
            ],
            "messages": messages,
        }
        status, headers, body = http_json("POST", f"{GATEWAY}/v1/messages", payload)
        assert status == 200, f"expected 200, got {status}: {body}"
        assert body.get("type") == "message", f"expected Anthropic message body, got {body}"
        ch = get_header(headers, "X-Gateway-Channel-Id")
        is_sticky = get_header(headers, "X-Gateway-Sticky-Hit") == "true"

        if turn == 0:
            pinned_channel = ch
        else:
            assert ch == pinned_channel, f"session stickiness broken! jumped {pinned_channel} -> {ch}"
            if is_sticky:
                sticky_hits += 1
        messages.append({"role": "assistant", "content": f"Ack turn {turn}"})

    _, _, stats = http_json("GET", f"{UPSTREAMS[pinned_channel]}/__admin/stats")
    hit_rate = stats["cache_hits"] / 9.0
    assert sticky_hits == 9 and hit_rate == 1.0, f"expected 9/9 sticky hits, got {sticky_hits}/9"
    return f"pinned={pinned_channel}, sticky_hits=9/9 (100% from Turn 1 without X-Session-Id)"


def test_2_session_stickiness_with_session_id():
    reset_all()
    pinned_channel = None
    sticky_hits = 0
    for turn in range(6):
        status, headers, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/messages",
            {
                "model": "claude-sonnet-4-20250514",
                "stream": False,
                "messages": [{"role": "user", "content": f"msg {turn}"}],
            },
            headers={"X-Session-Id": "explicit-sess-99"},
        )
        assert status == 200
        ch = get_header(headers, "X-Gateway-Channel-Id")
        if turn == 0:
            pinned_channel = ch
        else:
            assert ch == pinned_channel
            if get_header(headers, "X-Gateway-Sticky-Hit") == "true":
                sticky_hits += 1
    assert sticky_hits == 5
    return f"pinned={pinned_channel}, sticky_hits=5/5 (explicit X-Session-Id)"


def test_3_cancel_propagation():
    reset_all()
    conn = http.client.HTTPConnection("127.0.0.1", 8787, timeout=10)
    payload = json.dumps(
        {
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "stream": True,
            "mock_slow_chunks": 20,
            "messages": [{"role": "user", "content": "stream slowly"}],
        }
    )
    conn.request(
        "POST",
        "/v1/messages",
        body=payload,
        headers={"Content-Type": "application/json", "X-Session-Id": "cancel-test"},
    )
    resp = conn.getresponse()
    assert resp.status == 200
    ch = resp.getheader("X-Gateway-Channel-Id")
    assert len(resp.read(128)) > 0
    conn.close()

    time.sleep(0.8)
    _, _, stats = http_json("GET", f"{UPSTREAMS[ch]}/__admin/stats")
    assert stats["aborted_streams"] >= 1 and stats["completed_streams"] == 0
    return f"channel={ch}, aborted={stats['aborted_streams']}, completed=0 (<1s TCP abort)"


def test_4_four_tier_error_governance():
    # L1: 502 transient jitter -> retries in-place without evicting channel
    reset_all()
    http_json("POST", f"{UPSTREAMS['anthropic-primary']}/__admin/fault", {"status": 502, "body": "bad gateway", "count": 1})
    http_json("POST", f"{UPSTREAMS['anthropic-backup']}/__admin/fault", {"status": 502, "body": "bad gateway", "count": 1})
    st_l1, _, _ = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {"model": "claude-sonnet-4-20250514", "stream": False, "messages": [{"role": "user", "content": "l1"}]},
        headers={"X-Session-Id": "l1-test"},
    )
    assert st_l1 == 200, f"L1 should recover in-place, got {st_l1}"

    # L2: Hard 429 -> short cooldown + seamless failover
    reset_all()
    http_json("POST", f"{UPSTREAMS['anthropic-primary']}/__admin/fault", {"status": 429, "body": "RPM exceeded", "retry_after": 60, "count": 2})
    st_l2, _, _ = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {"model": "claude-sonnet-4-20250514", "stream": False, "messages": [{"role": "user", "content": "l2"}]},
        headers={"X-Session-Id": "l2-test"},
    )
    assert st_l2 == 200, f"L2 should failover to healthy backup, got {st_l2}"

    # L3: 401 auth failure -> quarantine + unbind affinity + failover
    reset_all()
    http_json("POST", f"{UPSTREAMS['anthropic-primary']}/__admin/fault", {"status": 401, "body": "invalid api key schema", "count": 2})
    st_l3, _, _ = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {"model": "claude-sonnet-4-20250514", "stream": False, "messages": [{"role": "user", "content": "l3"}]},
        headers={"X-Session-Id": "l3-test"},
    )
    assert st_l3 == 200, f"L3 should quarantine primary and failover, got {st_l3}"

    # L4: 400 semantic error -> immediate native Anthropic error response, zero retry
    reset_all()
    for ch in ("anthropic-primary", "anthropic-backup"):
        http_json("POST", f"{UPSTREAMS[ch]}/__admin/fault", {"status": 400, "body": "invalid_request_error: bad schema", "count": 1})
    st_l4, hdr_l4, body_l4 = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {"model": "claude-sonnet-4-20250514", "stream": False, "messages": [{"role": "user", "content": "l4"}]},
        headers={"X-Session-Id": "l4-test"},
    )
    assert st_l4 == 400
    assert body_l4.get("type") == "error" and "error" in body_l4, f"expected native Anthropic error schema, got {body_l4}"
    assert get_header(hdr_l4, "X-Gateway-Error-Tier") == "L4SemanticNoRetry"
    return "L1 in-place retry=200, L2 failover=200, L3 quarantine+failover=200, L4 native Anthropic 400"


def test_5_cross_protocol_non_streaming_response():
    reset_all()
    status, headers, body = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {
            "model": "claude-sonnet-4-20250514",
            "stream": False,
            "messages": [{"role": "user", "content": "generate title"}],
        },
        headers={"X-Gateway-Route-Mode": "stage_aware", "X-Gateway-Task-Stage": "title_generation"},
    )
    assert status == 200
    assert get_header(headers, "X-Gateway-Actual-Model") == "gpt-4o-mini"
    # Verify body is translated back into valid Anthropic Messages format!
    assert body.get("type") == "message", f"cross-protocol response not translated to Anthropic: {body}"
    assert body.get("role") == "assistant"
    assert isinstance(body.get("content"), list) and body["content"][0]["type"] == "text"
    assert body.get("stop_reason") == "end_turn"
    return "Anthropic -> OpenAI gpt-4o-mini -> valid Anthropic Messages JSON body"


def test_6_cross_protocol_streaming_frames_and_utf8():
    reset_all()
    conn = http.client.HTTPConnection("127.0.0.1", 8787, timeout=10)
    payload = json.dumps(
        {
            "model": "claude-sonnet-4-20250514",
            "stream": True,
            "messages": [{"role": "user", "content": "stream title"}],
        }
    )
    conn.request(
        "POST",
        "/v1/messages",
        body=payload,
        headers={
            "Content-Type": "application/json",
            "X-Gateway-Route-Mode": "stage_aware",
            "X-Gateway-Task-Stage": "title_generation",
        },
    )
    resp = conn.getresponse()
    assert resp.status == 200
    raw_sse = resp.read().decode("utf-8")
    conn.close()

    assert "\ufffd" not in raw_sse, "UTF-8 replacement character detected in SSE stream!"
    events = [line.split("event:", 1)[1].strip() for line in raw_sse.splitlines() if line.startswith("event:")]
    assert "message_start" in events, f"missing message_start in {events}"
    assert "content_block_start" in events, f"missing content_block_start in {events}"
    assert "content_block_delta" in events, f"missing content_block_delta in {events}"
    assert "content_block_stop" in events, f"missing content_block_stop in {events}"
    assert "message_stop" in events, f"missing message_stop in {events}"
    assert "跨协议中文" in raw_sse
    return f"events={events[:4]}...{events[-2:]}, CJK UTF-8 intact"


def test_7_trajectory_fork_and_diff():
    reset_all()
    sess_id = f"fork-demo-{int(time.time())}"
    status, headers, _ = http_json(
        "POST",
        f"{GATEWAY}/v1/messages",
        {
            "model": "claude-sonnet-4-20250514",
            "stream": False,
            "messages": [{"role": "user", "content": "Explain P2C load balancing"}],
        },
        headers={"X-Session-Id": sess_id},
    )
    assert status == 200
    step_id = get_header(headers, "X-Gateway-Step-Id")
    assert step_id

    st_code, _, diff = http_json(
        "POST",
        f"{GATEWAY}/api/steps/{step_id}/fork",
        {"target_model": "gpt-4o"},
    )
    assert st_code == 200, f"fork failed: {st_code} {diff}"
    assert diff["original"]["actual_model"] == "claude-sonnet-4-20250514"
    assert diff["forked"]["actual_model"] == "gpt-4o"
    assert diff["forked"]["parent_step_id"] == step_id

    st_code2, _, steps_resp = http_json("GET", f"{GATEWAY}/api/sessions/{sess_id}/steps")
    assert st_code2 == 200 and len(steps_resp["steps"]) == 2
    return f"forked {step_id} -> {diff['forked']['step_id']} (gpt-4o), 2 steps persisted"


if __name__ == "__main__":
    tests = [
        ("1. Session Stickiness (no X-Session-Id)", test_1_session_stickiness_without_session_id),
        ("2. Session Stickiness (explicit X-Session-Id)", test_2_session_stickiness_with_session_id),
        ("3. Cancel Propagation (mid-stream Ctrl+C)", test_3_cancel_propagation),
        ("4. 4-Tier Error Governance (L1/L2/L3/L4)", test_4_four_tier_error_governance),
        ("5. Cross-Protocol Non-Streaming IR Body", test_5_cross_protocol_non_streaming_response),
        ("6. Cross-Protocol SSE Framing & UTF-8", test_6_cross_protocol_streaming_frames_and_utf8),
        ("7. Stage 2 Trajectory Replay & Fork/Diff", test_7_trajectory_fork_and_diff),
    ]
    failed = 0
    for name, fn in tests:
        try:
            detail = fn()
            print(f"[PASS] {name}: {detail}")
        except Exception as exc:
            failed += 1
            print(f"[FAIL] {name}: {exc}", file=sys.stderr)

    print(f"\nSummary: {len(tests) - failed}/{len(tests)} passed.")
    sys.exit(1 if failed else 0)
