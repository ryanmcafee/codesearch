#!/usr/bin/env node
// scripts/bench-live.mjs -- search latency SLO bench against a running `codesearch serve`.
//
// Read-only: issues MCP `search` tool calls over streamable HTTP, one session per call.
//
// Usage:
//   node scripts/bench-live.mjs [--url http://127.0.0.1:39725] [--runs 20] [--project <alias>]
//                               [--query "..."] [--slo-ms 5000] [--timeout-ms 60000]
//
// Exit code 0 when every scope meets the p95 SLO, 1 otherwise.

import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

function parseArgs(argv) {
  const args = { runs: 20, query: 'error handling retry', sloMs: 5000, timeoutMs: 60000 };
  for (let i = 0; i < argv.length; i += 1) {
    const [key, value] = [argv[i], argv[i + 1]];
    switch (key) {
      case '--url': args.url = value; i += 1; break;
      case '--runs': args.runs = Number(value); i += 1; break;
      case '--project': args.project = value; i += 1; break;
      case '--query': args.query = value; i += 1; break;
      case '--slo-ms': args.sloMs = Number(value); i += 1; break;
      case '--timeout-ms': args.timeoutMs = Number(value); i += 1; break;
      default: throw new Error(`unknown option: ${key}`);
    }
  }
  if (!args.url) {
    try {
      args.url = readFileSync(join(homedir(), '.codesearch', 'serve_url'), 'utf8').trim();
    } catch {
      args.url = 'http://127.0.0.1:39725';
    }
  }
  return args;
}

async function http(method, url, { body, headers = {}, timeoutMs }) {
  const response = await fetch(url, {
    method,
    headers: body ? { 'content-type': 'application/json', ...headers } : headers,
    body: body ? JSON.stringify(body) : undefined,
    signal: AbortSignal.timeout(timeoutMs),
  });
  return { status: response.status, headers: response.headers, text: await response.text() };
}

function parseRpc(text) {
  const payload = text.includes('data:')
    ? text
        .split('\n')
        .filter((line) => line.startsWith('data:'))
        .map((line) => line.slice(5).trim())
        .pop()
    : text;
  return JSON.parse(payload);
}

async function mcpSearch(base, searchArgs, timeoutMs) {
  const accept = { accept: 'application/json, text/event-stream' };
  let sessionId = null;
  try {
    const init = await http('POST', `${base}/mcp`, {
      headers: accept,
      timeoutMs,
      body: {
        jsonrpc: '2.0',
        id: 1,
        method: 'initialize',
        params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'cs-bench-live', version: '1' } },
      },
    });
    sessionId = init.headers.get('mcp-session-id');
    if (init.status !== 200 || !sessionId) throw new Error(`initialize -> ${init.status}`);
    const session = { ...accept, 'mcp-session-id': sessionId };
    await http('POST', `${base}/mcp`, {
      headers: session,
      timeoutMs,
      body: { jsonrpc: '2.0', method: 'notifications/initialized' },
    });
    const started = performance.now();
    const call = await http('POST', `${base}/mcp`, {
      headers: session,
      timeoutMs,
      body: { jsonrpc: '2.0', id: 2, method: 'tools/call', params: { name: 'search', arguments: searchArgs } },
    });
    const ms = performance.now() - started;
    const rpc = parseRpc(call.text);
    if (rpc.error) throw new Error(`search error: ${rpc.error.message}`);
    if (rpc.result?.isError) throw new Error(`tool error: ${JSON.stringify(rpc.result.content).slice(0, 200)}`);
    return { ok: true, ms };
  } catch (error) {
    return { ok: false, error: error.name === 'TimeoutError' ? `timeout after ${timeoutMs}ms` : error.message };
  } finally {
    if (sessionId) {
      await http('DELETE', `${base}/mcp`, { headers: { 'mcp-session-id': sessionId }, timeoutMs: 2000 }).catch(() => {});
    }
  }
}

function percentile(sorted, p) {
  if (sorted.length === 0) return NaN;
  const rank = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, rank)];
}

async function firstProject(base, timeoutMs) {
  const res = await http('GET', `${base}/status`, { timeoutMs });
  const status = JSON.parse(res.text);
  const repos = status.repos ?? status.projects ?? [];
  const names = Array.isArray(repos) ? repos.map((r) => r.alias ?? r.name).filter(Boolean) : Object.keys(repos);
  if (names.length === 0) throw new Error('no registered repos in /status; pass --project');
  return names.sort()[0];
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const project = args.project ?? (await firstProject(args.url, args.timeoutMs));
  const scopes = [
    { name: 'unscoped semantic', args: { query: args.query, mode: 'semantic' } },
    { name: 'unscoped literal', args: { query: args.query, mode: 'literal' } },
    { name: `project=${project} semantic`, args: { query: args.query, mode: 'semantic', project } },
  ];

  console.log(`serve: ${args.url}  runs/scope: ${args.runs}  SLO p95 <= ${args.sloMs}ms`);
  let allPass = true;
  for (const scope of scopes) {
    const samples = [];
    const errors = [];
    for (let i = 0; i < args.runs; i += 1) {
      const result = await mcpSearch(args.url, scope.args, args.timeoutMs);
      if (result.ok) samples.push(result.ms);
      else errors.push(result.error);
    }
    samples.sort((a, b) => a - b);
    const p95 = percentile(samples, 95);
    const pass = errors.length === 0 && p95 <= args.sloMs;
    allPass &&= pass;
    const fmt = (v) => `${Math.round(v)}ms`.padStart(8);
    console.log(
      `${pass ? 'PASS' : 'FAIL'}  ${scope.name.padEnd(36)} p50${fmt(percentile(samples, 50))} p95${fmt(p95)}` +
        ` p99${fmt(percentile(samples, 99))} max${fmt(samples.at(-1) ?? NaN)}  errors ${errors.length}`,
    );
    for (const error of new Set(errors)) console.log(`      error: ${error}`);
  }
  process.exit(allPass ? 0 : 1);
}

main().catch((error) => {
  console.error(`bench-live: ${error.message}`);
  process.exit(1);
});
