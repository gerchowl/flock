#!/usr/bin/env python3
"""Minimal stdio MCP server for the S0 probes: one tool, `echo`, that
returns its `text` argument. Newline-delimited JSON-RPC, stdlib only."""

import json
import sys

TOOLS = [{
    "name": "echo",
    "description": "Return the given text unchanged.",
    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]},
}]


def reply(mid, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method, mid = msg.get("method"), msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        reply(mid, {
            "protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "s0stub", "version": "0"},
        })
    elif method == "tools/list":
        reply(mid, {"tools": TOOLS})
    elif method == "tools/call":
        text = msg.get("params", {}).get("arguments", {}).get("text", "")
        reply(mid, {"content": [{"type": "text", "text": text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no such method"}}) + "\n")
        sys.stdout.flush()
