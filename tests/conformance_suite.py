#!/usr/bin/env python3
"""
Edge Gateway v0.2.1 Automated Conformance & Stress Suite (Isolated Runner).

Every test resets both the mock upstreams AND the gateway's in-memory sticky/cooldown
state via POST /__admin/reset so tests never pollute each other, and all 7 tests execute
independently before reporting the final scorecard.
"""

import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import sys
import threading
import time
import urllib.error
import urllib.request

GATEWAY = "http://127.0.0.1:8787"
CLOUD_CONTROL_PLANE = "http://127.0.0.1:8790"
UPSTREAMS = {
    "anthropic-primary": "http://127.0.0.1:18081",
    "anthropic-backup": "http://127.0.0.1:18082",
    "openai-primary": "http://127.0.0.1:18083",
    "gemini-primary": "http://127.0.0.1:18084",
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
    assert events.count("message_stop") == 1, f"duplicate message_stop detected in {events}"
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


def test_8_gemini_native_and_three_protocol_interop():
    reset_all()

    # 8a. Native Gemini -> Gemini upstream
    st1, h1, b1 = http_json(
        "POST",
        f"{GATEWAY}/v1beta/models/gemini-2.5-pro:generateContent",
        {
            "contents": [{"role": "user", "parts": [{"text": "Hello Gemini native"}]}],
            "generationConfig": {"maxOutputTokens": 256},
        },
    )
    assert st1 == 200, f"native gemini failed: {st1} {b1}"
    assert get_header(h1, "X-Gateway-Actual-Model") == "gemini-2.5-pro"
    parts1 = b1["candidates"][0]["content"]["parts"]
    assert any("Gemini reply from gemini-primary" in p.get("text", "") for p in parts1), f"unexpected gemini body: {b1}"

    # 8b. Gemini client -> OpenAI upstream (cross-protocol non-streaming)
    st2, h2, b2 = http_json(
        "POST",
        f"{GATEWAY}/v1beta/models/gpt-4o-mini:generateContent",
        {
            "contents": [{"role": "user", "parts": [{"text": "Gemini to OpenAI"}]}],
        },
    )
    assert st2 == 200, f"gemini->openai failed: {st2} {b2}"
    assert get_header(h2, "X-Gateway-Actual-Model") == "gpt-4o-mini"
    assert "candidates" in b2 and b2["candidates"][0]["finishReason"] == "STOP"
    assert "OpenAI reply from openai-primary" in b2["candidates"][0]["content"]["parts"][0]["text"]

    # 8c. OpenAI client -> Gemini upstream (cross-protocol non-streaming)
    st3, h3, b3 = http_json(
        "POST",
        f"{GATEWAY}/v1/chat/completions",
        {
            "model": "gemini-2.5-pro",
            "stream": False,
            "messages": [{"role": "user", "content": "OpenAI to Gemini"}],
        },
    )
    assert st3 == 200, f"openai->gemini failed: {st3} {b3}"
    assert get_header(h3, "X-Gateway-Actual-Model") == "gemini-2.5-pro"
    assert b3.get("object") == "chat.completion"
    assert "Gemini reply from gemini-primary" in b3["choices"][0]["message"]["content"]

    # 8d. Anthropic client -> Gemini upstream (cross-protocol SSE streaming + CJK UTF-8)
    conn = http.client.HTTPConnection("127.0.0.1", 8787, timeout=10)
    conn.request(
        "POST",
        "/v1/messages",
        body=json.dumps(
            {
                "model": "gemini-2.5-pro",
                "stream": True,
                "messages": [{"role": "user", "content": "Anthropic stream to Gemini"}],
            }
        ),
        headers={"Content-Type": "application/json"},
    )
    resp = conn.getresponse()
    assert resp.status == 200, f"anthropic->gemini stream failed: {resp.status}"
    raw_sse = resp.read().decode("utf-8")
    conn.close()
    assert "\ufffd" not in raw_sse
    events = [line.split("event:", 1)[1].strip() for line in raw_sse.splitlines() if line.startswith("event:")]
    assert "message_start" in events and "content_block_start" in events and "message_stop" in events
    assert "Gemini中文分片" in raw_sse

    return "Gemini native + Gemini<->OpenAI + Anthropic->Gemini SSE all green"


def test_9_gemini_thought_signature_sidecar_restoration():
    reset_all()
    sess_id = f"gem-sig-{int(time.time())}"

    # Turn 1: Gemini upstream returns thoughtSignature = sig_gemini_1, cached by GatewayEngine
    st1, _, b1 = http_json(
        "POST",
        f"{GATEWAY}/v1beta/models/gemini-2.5-pro:generateContent",
        {
            "contents": [{"role": "user", "parts": [{"text": "Turn 1 reasoning"}]}],
            "generationConfig": {"thinkingConfig": {"includeThoughts": True, "thinkingBudget": 1024}},
        },
        headers={"X-Session-Id": sess_id},
    )
    assert st1 == 200, f"turn 1 failed: {st1} {b1}"
    turn1_sig = b1["candidates"][0]["content"]["parts"][0].get("thoughtSignature")
    assert turn1_sig == "sig_gemini_1", f"expected sig_gemini_1, got {turn1_sig}"

    # Turn 2: Client strips thoughtSignature from history; gateway must hydrate it from SignatureCache
    st2, _, b2 = http_json(
        "POST",
        f"{GATEWAY}/v1beta/models/gemini-2.5-pro:generateContent",
        {
            "contents": [
                {"role": "user", "parts": [{"text": "Turn 1 reasoning"}]},
                {
                    "role": "model",
                    "parts": [
                        {"thought": True, "text": "Gemini internal reasoning"},
                        {"text": "Gemini reply from gemini-primary (gemini-2.5-pro)"},
                    ],
                },
                {"role": "user", "parts": [{"text": "Turn 2 follow-up"}]},
            ],
        },
        headers={"X-Session-Id": sess_id},
    )
    assert st2 == 200, f"turn 2 failed: {st2} {b2}"

    _, _, stats = http_json("GET", f"{UPSTREAMS['gemini-primary']}/__admin/stats")
    last_req = stats.get("last_request") or {}
    restored_sig = last_req["contents"][1]["parts"][0].get("thoughtSignature")
    assert restored_sig == "sig_gemini_1", f"expected hydrated sig_gemini_1, got {restored_sig} in {last_req}"
    return f"stripped thoughtSignature transparently restored as '{restored_sig}' on Turn 2"


def test_10_onion_middleware_pipeline_and_outbound_guardrail():
    reset_all()
    st_mw, _, mw_info = http_json("GET", f"{GATEWAY}/api/middleware")
    assert st_mw == 200, f"/api/middleware failed: {st_mw} {mw_info}"
    expected_layers = ["ingress_rate_limit", "guardrail", "context_memory", "mcp_bridge"]
    assert mw_info.get("layers") == expected_layers, f"unexpected layers: {mw_info}"

    # Send a prompt containing an API key & email -> verify Guardrail redacts before upstream sees it
    st, headers, _ = http_json(
        "POST",
        f"{GATEWAY}/v1/chat/completions",
        {
            "model": "gpt-4o-mini",
            "stream": False,
            "messages": [
                {
                    "role": "user",
                    "content": "My key is sk-ant-secret1234567890 and email is dev@loomgate.dev",
                }
            ],
        },
        headers={"X-Session-Id": "mw-guardrail-test"},
    )
    assert st == 200, f"request failed: {st}"
    assert get_header(headers, "X-Gateway-Guardrail-Redactions") == "2", f"missing redaction header: {headers}"

    _, _, stats = http_json("GET", f"{UPSTREAMS['openai-primary']}/__admin/stats")
    sent_msg = stats["last_request"]["messages"][0]["content"]
    assert "sk-ant-secret" not in sent_msg and "[REDACTED_SECRET]" in sent_msg, f"secret leaked: {sent_msg}"
    assert "dev@loomgate.dev" not in sent_msg and "[REDACTED_EMAIL]" in sent_msg, f"email leaked: {sent_msg}"
    return f"layers={expected_layers}, 2 secrets/PII redacted before upstream"


def test_11_edge_cloud_hybrid_login_billing_and_sanitized_telemetry():
    reset_all()
    # 11a. Login directly to Cloud Control Plane to register a team-only model channel
    st_cp, _, cp_login = http_json(
        "POST",
        f"{CLOUD_CONTROL_PLANE}/api/v1/auth/login",
        {"email": "dev@loomgate.dev", "password": "loomgate-dev-password"},
    )
    assert st_cp == 200, f"cloud control plane unreachable ({st_cp}): {cp_login}"
    cp_token = cp_login["token"]
    cp_auth = {"Authorization": f"Bearer {cp_token}"}

    st_reg, _, reg_body = http_json(
        "POST",
        f"{CLOUD_CONTROL_PLANE}/api/v1/admin/channels",
        {
            "channel": {
                "id": "team-cloud-openai",
                "protocol": "openai",
                "base_url": "http://127.0.0.1:18083",
                "api_key": "sk-team-pool-key",
                "weight": 100,
                "models": ["gpt-4o-team"],
            }
        },
        headers=cp_auth,
    )
    assert st_reg == 200, f"failed to register team channel: {st_reg} {reg_body}"

    # 11b. Edge daemon logs in -> hot-merges team channel pool without restart
    st_edge, _, edge_login = http_json(
        "POST",
        f"{GATEWAY}/api/cloud/login",
        {
            "control_plane_url": CLOUD_CONTROL_PLANE,
            "email": "dev@loomgate.dev",
            "password": "loomgate-dev-password",
        },
    )
    assert st_edge == 200 and edge_login["cloud"]["connected"], f"edge cloud login failed: {st_edge} {edge_login}"
    assert edge_login["cloud"]["synced_channels"] >= 1

    try:
        # 11c. Send request targeting the team-only model with a private sentinel string
        sentinel = "CONFIDENTIAL_LOCAL_PROMPT_XYZ_9988"
        st_req, h_req, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/chat/completions",
            {
                "model": "gpt-4o-team",
                "stream": False,
                "messages": [{"role": "user", "content": f"Analyze {sentinel}"}],
            },
            headers={"X-Session-Id": "cloud-hybrid-sess"},
        )
        assert st_req == 200, f"team-pool routed request failed: {st_req}"
        assert get_header(h_req, "X-Gateway-Channel-Id") == "team-cloud-openai"

        # 11d. Verify atomic 2-phase reserve/settle closed cleanly with 0 leaked reservation
        st_bal, _, bal = http_json("GET", f"{CLOUD_CONTROL_PLANE}/api/v1/billing/balance", headers=cp_auth)
        assert st_bal == 200
        assert bal["reserved_micros"] == 0, f"unreleased billing reservation: {bal}"
        assert bal["total_settled_micros"] > 0, f"expected settled charge > 0: {bal}"

        # 11e. Verify ClickHouse-compatible telemetry ingested metadata with ZERO raw prompt text
        st_ana, _, ana = http_json("GET", f"{CLOUD_CONTROL_PLANE}/api/v1/telemetry/analytics", headers=cp_auth)
        assert st_ana == 200
        assert ana["summary"]["total_events"] >= 1
        raw_ana = json.dumps(ana)
        assert sentinel not in raw_ana, "PRIVACY BREACH: raw prompt leaked to cloud telemetry!"
        assert "cloud-hybrid-sess" not in raw_ana, "PRIVACY BREACH: unhashed session_id leaked to cloud!"

        return (
            f"team channel hot-merged, settled={bal['total_settled_micros']} micro-USD "
            f"(reserved=0), sanitized events={ana['summary']['total_events']} (0 raw text leaked)"
        )
    finally:
        http_json("POST", f"{GATEWAY}/api/cloud/logout")


def test_12_claude_cli_preflight_count_tokens():
    reset_all()
    st, _, body = http_json(
        "POST",
        f"{GATEWAY}/v1/messages/count_tokens",
        {
            "model": "claude-sonnet-4-20250514",
            "system": "You are LoomGate coding assistant.",
            "messages": [{"role": "user", "content": "Count tokens for this Claude CLI preflight check."}],
        },
    )
    assert st == 200, f"count_tokens failed: {st} {body}"
    assert isinstance(body.get("input_tokens"), int) and body["input_tokens"] > 0, f"invalid count_tokens: {body}"
    return f"input_tokens={body['input_tokens']}"


def test_13_declarative_client_registry_and_live_channel_crud():
    reset_all()
    # 13a. Client Profile Registry (3 atomic + 5 setup wizards)
    st_c, _, clients_body = http_json("GET", f"{GATEWAY}/api/clients")
    assert st_c == 200, f"/api/clients failed: {st_c} {clients_body}"
    ids = {p["id"] for p in clients_body.get("profiles", [])}
    required = {
        "claude_code",
        "codex",
        "gemini_cli",
        "cursor",
        "cline_roo",
        "cherry_studio",
        "trae",
        "generic_sdk",
    }
    assert required.issubset(ids), f"missing client profiles: {required - ids}"

    # 13b. Live Channel Upsert with redundant /v1/ -> auto-normalized + Test Connection + Toggle + Delete
    draft = {
        "id": "dynamic-test-ch",
        "protocol": "openai",
        "base_url": "http://127.0.0.1:18083/v1/",
        "api_key": "sk-dyn-test",
        "weight": 100,
        "enabled": True,
        "models": ["gpt-4o-dyn"],
        "fallback_models": ["gpt-4o-mini"],
        "quota_endpoint": "http://127.0.0.1:18083/__admin/stats",
    }
    st_up, _, up_body = http_json("POST", f"{GATEWAY}/api/channels", draft)
    assert st_up == 200 and up_body["ok"], f"upsert failed: {st_up} {up_body}"
    assert up_body["channel"]["base_url"] == "http://127.0.0.1:18083", f"base_url not normalized: {up_body}"

    st_probe, _, probe = http_json("POST", f"{GATEWAY}/api/channels/test", draft)
    assert st_probe == 200 and probe["ok"], f"channel probe failed: {st_probe} {probe}"
    assert probe.get("quota_info") is not None, f"expected quota_info from quota_endpoint: {probe}"

    st_tog, _, tog = http_json("POST", f"{GATEWAY}/api/channels/dynamic-test-ch/toggle", {"enabled": False})
    assert st_tog == 200 and tog["channel"]["enabled"] is False

    st_del, _, del_body = http_json("DELETE", f"{GATEWAY}/api/channels/dynamic-test-ch")
    assert st_del == 200 and del_body["deleted"] is True

    return f"8 built-in profiles verified; base_url normalized; probe={probe['latency_ms']}ms with quota_info"


def test_14_split_api_key_and_admin_password_auth():
    reset_all()
    api_secret = "lg_api_split_test"
    admin_secret = "lg_admin_split_test"

    st_set, _, set_body = http_json(
        "POST",
        f"{GATEWAY}/api/auth/settings",
        {"api_key": api_secret, "admin_password": admin_secret},
    )
    assert st_set == 200 and set_body["ok"], f"failed to set split auth: {st_set} {set_body}"

    try:
        # Inference rejects missing key and admin password; accepts api_key
        st_no_key, _, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/messages/count_tokens",
            {"model": "claude-sonnet-4-20250514", "messages": [{"role": "user", "content": "hi"}]},
        )
        assert st_no_key == 401, f"expected 401 without api_key, got {st_no_key}"

        st_wrong_key, _, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/messages/count_tokens",
            {"model": "claude-sonnet-4-20250514", "messages": [{"role": "user", "content": "hi"}]},
            headers={"x-api-key": admin_secret},
        )
        assert st_wrong_key == 401, f"admin_password must NOT unlock inference: got {st_wrong_key}"

        st_ok_key, _, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/messages/count_tokens",
            {"model": "claude-sonnet-4-20250514", "messages": [{"role": "user", "content": "hi"}]},
            headers={"x-api-key": api_secret},
        )
        assert st_ok_key == 200, f"expected 200 with api_key, got {st_ok_key}"

        # Admin rejects api_key; accepts admin_password
        st_adm_deny, _, _ = http_json(
            "GET", f"{GATEWAY}/api/channels", headers={"Authorization": f"Bearer {api_secret}"}
        )
        assert st_adm_deny == 401, f"api_key must NOT unlock /api/* admin: got {st_adm_deny}"

        st_adm_ok, _, _ = http_json(
            "GET", f"{GATEWAY}/api/channels", headers={"x-admin-password": admin_secret}
        )
        assert st_adm_ok == 200, f"expected 200 with admin_password, got {st_adm_ok}"

        return "api_key (/v1/*) & admin_password (/api/*) strictly isolated"
    finally:
        # Always restore passwordless local mode so subsequent runs stay clean
        http_json(
            "POST",
            f"{GATEWAY}/api/auth/settings",
            {"api_key": None, "admin_password": None},
            headers={"x-admin-password": admin_secret},
        )


def test_15_local_model_discovery_import_and_offline_fallback():
    reset_all()

    class MockOllamaHandler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def do_GET(self):
            if self.path == "/api/tags":
                body = json.dumps({"models": [{"name": "qwen2.5-coder:7b"}]}).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_response(404)
                self.end_headers()

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length) if length > 0 else b"{}"
            req = json.loads(raw.decode("utf-8"))
            body = json.dumps(
                {
                    "id": "chatcmpl_local_ollama_1",
                    "object": "chat.completion",
                    "model": req.get("model", "qwen2.5-coder:7b"),
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "Offline Ollama reply"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25},
                }
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    class MockLmStudioHandler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def do_GET(self):
            if self.path == "/v1/models":
                body = json.dumps({"object": "list", "data": [{"id": "llama-3.3-70b-instruct"}]}).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            else:
                self.send_response(404)
                self.end_headers()

    ol_srv = ThreadingHTTPServer(("127.0.0.1", 0), MockOllamaHandler)
    lm_srv = ThreadingHTTPServer(("127.0.0.1", 0), MockLmStudioHandler)
    ol_port = ol_srv.server_address[1]
    lm_port = lm_srv.server_address[1]
    threading.Thread(target=ol_srv.serve_forever, daemon=True).start()
    threading.Thread(target=lm_srv.serve_forever, daemon=True).start()

    imported_id = None
    try:
        t0 = time.monotonic()
        st, _, body = http_json("GET", f"{GATEWAY}/api/local/discover?ports={ol_port},{lm_port}")
        elapsed_ms = int((time.monotonic() - t0) * 1000)
        assert st == 200, f"local discover failed: {st} {body}"
        providers = body.get("providers", [])
        by_port = {p["port"]: p for p in providers}
        assert ol_port in by_port and "qwen2.5-coder:7b" in by_port[ol_port]["models"], f"ollama missing: {providers}"
        assert lm_port in by_port and "llama-3.3-70b-instruct" in by_port[lm_port]["models"], f"lmstudio missing: {providers}"

        st_imp, _, imp_body = http_json(
            "POST",
            f"{GATEWAY}/api/local/import",
            {"provider": by_port[ol_port], "offline_fallback": True},
        )
        assert st_imp == 200 and imp_body["ok"], f"local import failed: {st_imp} {imp_body}"
        imported_id = imp_body["channel"]["id"]
        assert imp_body["channel"]["offline_fallback"] is True

        # Trigger offline fallback by requesting a cloud model with zero healthy cloud channels
        st_fb, h_fb, b_fb = http_json(
            "POST",
            f"{GATEWAY}/v1/chat/completions",
            {
                "model": "unavailable-cloud-model-xyz",
                "stream": False,
                "messages": [{"role": "user", "content": "Are we offline?"}],
            },
        )
        assert st_fb == 200, f"offline fallback request failed: {st_fb} {b_fb}"
        assert get_header(h_fb, "X-Gateway-Offline-Fallback") == "true"
        assert get_header(h_fb, "X-Gateway-Actual-Model") == "qwen2.5-coder:7b"
        assert "Offline Ollama reply" in b_fb["choices"][0]["message"]["content"]
        return f"discovered Ollama+LM Studio in {elapsed_ms}ms; offline_fallback routed -> qwen2.5-coder:7b"
    finally:
        if imported_id:
            http_json("DELETE", f"{GATEWAY}/api/channels/{imported_id}")
        ol_srv.shutdown()
        lm_srv.shutdown()
        ol_srv.server_close()
        lm_srv.server_close()


def test_16_local_savings_and_stop_loss_analytics():
    st, _, sav = http_json("GET", f"{GATEWAY}/api/analytics/savings?days=30")
    assert st == 200, f"savings endpoint failed: {st} {sav}"
    assert sav["total_requests"] >= 10, f"expected >=10 recorded requests, got {sav}"
    assert sav["total_cache_read_tokens"] > 0, f"expected cache_read_tokens > 0: {sav}"
    assert sav["avg_cache_hit_ratio"] > 0.0, f"expected positive cache hit ratio: {sav}"
    assert sav["aborted_by_client_count"] >= 1, f"expected >=1 Ctrl+C abort recorded: {sav}"
    assert sav["estimated_cache_savings_usd"] > 0.0, f"expected positive cache savings USD: {sav}"
    assert sav["estimated_stop_loss_savings_usd"] > 0.0, f"expected positive stop-loss savings USD: {sav}"
    assert len(sav["by_model"]) >= 2, f"expected multi-model breakdown: {sav}"
    return (
        f"reqs={sav['total_requests']}, cache_hit={sav['avg_cache_hit_ratio']*100:.1f}%, "
        f"aborts={sav['aborted_by_client_count']}, cache_saved=${sav['estimated_cache_savings_usd']:.5f}"
    )


def test_17_trajectory_studio_fork_diff_markdown_export_redacts_secrets():
    reset_all()

    # Local ephemeral upstream returning a response containing sk- & Bearer secrets
    class SecretEchoHandler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get("Content-Length", "0"))
            _ = self.rfile.read(length) if length > 0 else b"{}"
            body = json.dumps(
                {
                    "id": "chatcmpl_secret_test",
                    "object": "chat.completion",
                    "model": "secret-export-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "Use sk-live-secret1234567890 and Authorization: Bearer tok_supersecret987654",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 18, "total_tokens": 68},
                }
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    srv = ThreadingHTTPServer(("127.0.0.1", 0), SecretEchoHandler)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    ch_id = "secret-export-ch"

    try:
        st_up, _, _ = http_json(
            "POST",
            f"{GATEWAY}/api/channels",
            {
                "id": ch_id,
                "protocol": "openai",
                "base_url": f"http://127.0.0.1:{port}",
                "api_key": "sk-dummy",
                "weight": 100,
                "enabled": True,
                "models": ["secret-export-model"],
            },
        )
        assert st_up == 200

        st1, h1, _ = http_json(
            "POST",
            f"{GATEWAY}/v1/chat/completions",
            {
                "model": "secret-export-model",
                "stream": False,
                "messages": [{"role": "user", "content": "Show config example"}],
            },
            headers={"X-Session-Id": f"export-sess-{int(time.time())}"},
        )
        assert st1 == 200
        orig_step = get_header(h1, "X-Gateway-Step-Id")
        assert orig_step

        st_fk, _, diff = http_json(
            "POST",
            f"{GATEWAY}/api/steps/{orig_step}/fork",
            {"target_model": "gpt-4o"},
        )
        assert st_fk == 200, f"fork failed: {st_fk} {diff}"
        fork_step = diff["forked"]["step_id"]

        st_exp, _, exp = http_json(
            "POST",
            f"{GATEWAY}/api/steps/export-diff",
            {
                "original_step_id": orig_step,
                "forked_step_id": fork_step,
                "redact_secrets": True,
            },
        )
        assert st_exp == 200, f"export-diff failed: {st_exp} {exp}"
        md = exp["markdown"]
        assert exp["redacted"] is True and exp["redactions_applied"] >= 2, f"expected >=2 redactions: {exp}"
        assert "sk-live-secret1234567890" not in md, "sk- secret leaked into exported Markdown!"
        assert "tok_supersecret987654" not in md, "Bearer token leaked into exported Markdown!"
        assert "[REDACTED_SECRET]" in md
        assert "## Performance & Token Comparison" in md and "```diff" in md
        return f"redactions={exp['redactions_applied']}, sk-/Bearer scrubbed from Markdown export"
    finally:
        http_json("DELETE", f"{GATEWAY}/api/channels/{ch_id}")
        srv.shutdown()
        srv.server_close()


if __name__ == "__main__":
    tests = [
        ("1. Session Stickiness (no X-Session-Id)", test_1_session_stickiness_without_session_id),
        ("2. Session Stickiness (explicit X-Session-Id)", test_2_session_stickiness_with_session_id),
        ("3. Cancel Propagation (mid-stream Ctrl+C)", test_3_cancel_propagation),
        ("4. 4-Tier Error Governance (L1/L2/L3/L4)", test_4_four_tier_error_governance),
        ("5. Cross-Protocol Non-Streaming IR Body", test_5_cross_protocol_non_streaming_response),
        ("6. Cross-Protocol SSE Framing & UTF-8", test_6_cross_protocol_streaming_frames_and_utf8),
        ("7. Stage 2 Trajectory Replay & Fork/Diff", test_7_trajectory_fork_and_diff),
        ("8. Gemini Native & 3-Protocol Interop", test_8_gemini_native_and_three_protocol_interop),
        ("9. Gemini thought_signature Sidecar Restoration", test_9_gemini_thought_signature_sidecar_restoration),
        ("10. Stage 3 Onion Middleware & Guardrail Redaction", test_10_onion_middleware_pipeline_and_outbound_guardrail),
        ("11. Stage 3 Edge-Cloud Sync, 2PC Billing & Sanitized Telemetry", test_11_edge_cloud_hybrid_login_billing_and_sanitized_telemetry),
        ("12. v0.4.0 Claude CLI Preflight count_tokens", test_12_claude_cli_preflight_count_tokens),
        ("13. v0.4.0 Declarative Client Registry & Live Channel CRUD/Probe", test_13_declarative_client_registry_and_live_channel_crud),
        ("14. v0.4.0 Split Auth (api_key vs admin_password)", test_14_split_api_key_and_admin_password_auth),
        ("15. v0.5.0 Local Model Auto-Discovery, Import & Offline Fallback", test_15_local_model_discovery_import_and_offline_fallback),
        ("16. v0.5.0 Local Savings & Stop-Loss Telemetry Dashboard", test_16_local_savings_and_stop_loss_analytics),
        ("17. v0.5.0 Trajectory Studio Fork/Diff Markdown Export & Redaction", test_17_trajectory_studio_fork_diff_markdown_export_redacts_secrets),
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
