#!/usr/bin/env python3
"""
Local Multi-Channel Mock Upstream Server for Stage 1 + Stage 2 Conformance Testing.

Starts three local HTTP servers matching config.example.toml:
  - 127.0.0.1:18081  anthropic-primary
  - 127.0.0.1:18082  anthropic-backup
  - 127.0.0.1:18083  openai-primary

Returns spec-compliant Anthropic Messages & OpenAI Chat Completions JSON + SSE frames,
including CJK UTF-8 content, content_block_start/stop, KV prefix tracking, fault injection,
and client-abort detection.
"""

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class ChannelState:
    def __init__(self, channel_id: str, protocol: str):
        self.channel_id = channel_id
        self.protocol = protocol
        self.lock = threading.Lock()
        self.seen_prefixes: set[str] = set()
        self.total_requests = 0
        self.cache_hits = 0
        self.aborted_streams = 0
        self.completed_streams = 0
        self.fault_queue: list[dict] = []
        self.last_request: dict | None = None

    def reset(self):
        with self.lock:
            self.seen_prefixes.clear()
            self.total_requests = 0
            self.cache_hits = 0
            self.aborted_streams = 0
            self.completed_streams = 0
            self.fault_queue.clear()
            self.last_request = None

    def stats(self) -> dict:
        with self.lock:
            return {
                "channel_id": self.channel_id,
                "protocol": self.protocol,
                "total_requests": self.total_requests,
                "cache_hits": self.cache_hits,
                "aborted_streams": self.aborted_streams,
                "completed_streams": self.completed_streams,
                "pending_faults": len(self.fault_queue),
                "last_request": self.last_request,
            }


def make_handler(state: ChannelState):
    class MockUpstreamHandler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            pass

        def _read_json(self) -> dict:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length) if length > 0 else b"{}"
            return json.loads(raw.decode("utf-8"))

        def _send_json(self, status: int, payload: dict, extra_headers: dict | None = None):
            body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if extra_headers:
                for k, v in extra_headers.items():
                    self.send_header(k, v)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/__admin/stats":
                self._send_json(200, state.stats())
                return
            self._send_json(404, {"error": "not found"})

        def do_POST(self):
            if self.path == "/__admin/reset":
                state.reset()
                self._send_json(200, {"ok": True})
                return

            if self.path == "/__admin/fault":
                spec = self._read_json()
                count = int(spec.get("count", 1))
                with state.lock:
                    for _ in range(count):
                        state.fault_queue.append(spec)
                self._send_json(200, {"queued": count})
                return

            payload = self._read_json()
            prefix_key = self.headers.get("X-Gateway-Prefix-Key", "none")

            with state.lock:
                state.total_requests += 1
                state.last_request = payload
                fault = state.fault_queue.pop(0) if state.fault_queue else None

            if fault:
                extra = {}
                if "retry_after" in fault:
                    extra["Retry-After"] = str(fault["retry_after"])
                self._send_json(
                    int(fault["status"]),
                    {"error": {"message": fault.get("body", "injected fault")}},
                    extra_headers=extra,
                )
                return

            with state.lock:
                cache_hit = prefix_key in state.seen_prefixes
                if cache_hit:
                    state.cache_hits += 1
                else:
                    state.seen_prefixes.add(prefix_key)

            if state.protocol == "gemini":
                path_no_query = self.path.split("?", 1)[0]
                model_action = path_no_query.rsplit("/", 1)[-1]
                if ":" in model_action:
                    model, action = model_action.rsplit(":", 1)
                else:
                    model, action = "gemini-2.5-pro", "generateContent"
                stream = action == "streamGenerateContent"
            else:
                model = payload.get("model", "unknown")
                stream = bool(payload.get("stream", False))
            slow_chunks = int(payload.get("mock_slow_chunks", 0))

            if not stream:
                if state.protocol == "anthropic":
                    self._send_json(
                        200,
                        {
                            "id": f"msg_{state.channel_id}_{state.total_requests}",
                            "type": "message",
                            "role": "assistant",
                            "model": model,
                            "content": [
                                {"type": "text", "text": f"Anthropic reply from {state.channel_id} ({model})"}
                            ],
                            "stop_reason": "end_turn",
                            "stop_sequence": None,
                            "usage": {
                                "input_tokens": 100 if cache_hit else 1000,
                                "cache_read_input_tokens": 900 if cache_hit else 0,
                                "output_tokens": 42,
                            },
                        },
                    )
                elif state.protocol == "gemini":
                    self._send_json(
                        200,
                        {
                            "responseId": f"gem_{state.channel_id}_{state.total_requests}",
                            "modelVersion": model,
                            "candidates": [
                                {
                                    "index": 0,
                                    "content": {
                                        "role": "model",
                                        "parts": [
                                            {
                                                "thought": True,
                                                "text": "Gemini internal reasoning",
                                                "thoughtSignature": f"sig_gemini_{state.total_requests}",
                                            },
                                            {
                                                "text": f"Gemini reply from {state.channel_id} ({model})",
                                            },
                                        ],
                                    },
                                    "finishReason": "STOP",
                                }
                            ],
                            "usageMetadata": {
                                "promptTokenCount": 1000,
                                "cachedContentTokenCount": 900 if cache_hit else 0,
                                "candidatesTokenCount": 38,
                                "totalTokenCount": 1038,
                            },
                        },
                    )
                else:
                    self._send_json(
                        200,
                        {
                            "id": f"chatcmpl_{state.channel_id}_{state.total_requests}",
                            "object": "chat.completion",
                            "model": model,
                            "choices": [
                                {
                                    "index": 0,
                                    "message": {
                                        "role": "assistant",
                                        "content": f"OpenAI reply from {state.channel_id} ({model})",
                                    },
                                    "finish_reason": "stop",
                                }
                            ],
                            "usage": {
                                "prompt_tokens": 1000,
                                "completion_tokens": 36,
                                "total_tokens": 1036,
                                "prompt_tokens_details": {
                                    "cached_tokens": 900 if cache_hit else 0
                                },
                            },
                        },
                    )
                return

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            chunks = slow_chunks if slow_chunks > 0 else 3
            delay = 0.25 if slow_chunks > 0 else 0.01

            try:
                if state.protocol == "anthropic":
                    self._write_sse(
                        "message_start",
                        {
                            "type": "message_start",
                            "message": {
                                "id": f"msg_{state.channel_id}",
                                "model": model,
                                "usage": {
                                    "input_tokens": 100 if cache_hit else 1000,
                                    "cache_read_input_tokens": 900 if cache_hit else 0,
                                    "output_tokens": 0,
                                },
                            },
                        },
                    )
                    self._write_sse(
                        "content_block_start",
                        {
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": {"type": "text", "text": ""},
                        },
                    )
                    for i in range(chunks):
                        time.sleep(delay)
                        self._write_sse(
                            "content_block_delta",
                            {
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "text_delta", "text": f"中文分片-{i} "},
                            },
                        )
                    self._write_sse("content_block_stop", {"type": "content_block_stop", "index": 0})
                    self._write_sse(
                        "message_delta",
                        {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
                    )
                    self._write_sse("message_stop", {"type": "message_stop"})
                elif state.protocol == "gemini":
                    self._write_sse(
                        None,
                        {
                            "candidates": [
                                {
                                    "index": 0,
                                    "content": {
                                        "role": "model",
                                        "parts": [
                                            {
                                                "thought": True,
                                                "text": "思考过程",
                                                "thoughtSignature": f"sig_gemini_stream_{state.total_requests}",
                                            }
                                        ],
                                    },
                                }
                            ]
                        },
                    )
                    for i in range(chunks):
                        time.sleep(delay)
                        self._write_sse(
                            None,
                            {
                                "candidates": [
                                    {
                                        "index": 0,
                                        "content": {
                                            "role": "model",
                                            "parts": [{"text": f"Gemini中文分片-{i} "}],
                                        },
                                    }
                                ]
                            },
                        )
                    self._write_sse(
                        None,
                        {
                            "candidates": [{"index": 0, "finishReason": "STOP"}],
                            "usageMetadata": {
                                "promptTokenCount": 1000,
                                "cachedContentTokenCount": 900 if cache_hit else 0,
                                "candidatesTokenCount": chunks * 5,
                                "totalTokenCount": 1000 + chunks * 5,
                            },
                        },
                    )
                else:
                    for i in range(chunks):
                        time.sleep(delay)
                        self._write_sse(
                            None,
                            {
                                "id": f"chatcmpl_{state.channel_id}",
                                "object": "chat.completion.chunk",
                                "model": model,
                                "choices": [
                                    {
                                        "index": 0,
                                        "delta": {"content": f"跨协议中文-{i} "},
                                        "finish_reason": None,
                                    }
                                ],
                            },
                        )
                    self._write_sse(
                        None,
                        {
                            "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                            "usage": {
                                "prompt_tokens": 1000,
                                "completion_tokens": chunks * 4,
                                "prompt_tokens_details": {"cached_tokens": 900 if cache_hit else 0},
                            },
                        },
                    )
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()

                with state.lock:
                    state.completed_streams += 1
            except (BrokenPipeError, ConnectionResetError, OSError):
                with state.lock:
                    state.aborted_streams += 1

        def _write_sse(self, event_name: str | None, data: dict):
            raw = ""
            if event_name:
                raw += f"event: {event_name}\n"
            raw += f"data: {json.dumps(data, ensure_ascii=False)}\n\n"
            self.wfile.write(raw.encode("utf-8"))
            self.wfile.flush()

    return MockUpstreamHandler


def start_mock_cluster():
    specs = [
        ("anthropic-primary", "anthropic", 18081),
        ("anthropic-backup", "anthropic", 18082),
        ("openai-primary", "openai", 18083),
        ("gemini-primary", "gemini", 18084),
    ]
    servers = []
    for cid, proto, port in specs:
        state = ChannelState(cid, proto)
        srv = ThreadingHTTPServer(("127.0.0.1", port), make_handler(state))
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        servers.append((cid, port, srv, state))
    return servers


if __name__ == "__main__":
    cluster = start_mock_cluster()
    print("Mock upstream cluster running:")
    for cid, port, _, _ in cluster:
        print(f"  - {cid}: http://127.0.0.1:{port}")
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        for _, _, srv, _ in cluster:
            srv.shutdown()
