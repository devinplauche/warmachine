#!/usr/bin/env python3
"""Minimal MCP stdio test server (Phase 5 dogfood fixture).

Speaks newline-delimited JSON-RPC. Answers `initialize` only when
the DEMO_TOKEN environment variable matches the expected value;
otherwise replies with an auth error — so a harness health check
proves the vault-injected env actually reached the server.
"""
import json
import os
import sys

EXPECTED = "phase5-token-123"

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        continue
    if req.get("method") == "initialize":
        if os.environ.get("DEMO_TOKEN") == EXPECTED:
            resp = {
                "jsonrpc": "2.0",
                "id": req.get("id"),
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                },
            }
        else:
            resp = {
                "jsonrpc": "2.0",
                "id": req.get("id"),
                "error": {"code": -32001, "message": "unauthorized: bad DEMO_TOKEN"},
            }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif req.get("method") == "notifications/initialized":
        continue
    else:
        resp = {
            "jsonrpc": "2.0",
            "id": req.get("id"),
            "error": {"code": -32601, "message": "method not found"},
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
