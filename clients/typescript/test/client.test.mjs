// The TypeScript client against an in-process stand-in for the server.
// `npm test` builds with tsc and runs this against the compiled dist/index.js.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { Kompressor, KompressorError } from "../dist/index.js";

// The fields the server's request structs accept; both refuse unknown fields.
const ALLOWED = {
  "/v1/compress": ["messages", "model", "provider", "request_id", "options", "frozen_message_count", "context"],
  "/v1/compress/raw": ["content", "content_type", "options", "request_id", "context", "source_tool", "file_path"],
  "/v1/retrieve": ["ref", "range", "request_id"],
  "/v1/detect": ["content", "role"],
};

const seen = [];
let server;
let url;

before(async () => {
  server = http.createServer((req, res) => {
    const reply = (status, body) => {
      res.writeHead(status, { "content-type": "application/json" });
      res.end(JSON.stringify(body));
    };
    if (req.method === "GET") {
      return req.url === "/healthz" ? reply(200, { status: "ok" }) : reply(404, {});
    }
    let data = "";
    req.on("data", (chunk) => (data += chunk));
    req.on("end", () => {
      const body = JSON.parse(data);
      seen.push({ path: req.url, headers: req.headers, body });
      if (req.headers.authorization === "Bearer wrong") {
        return reply(401, { error: { code: "unauthorized", message: "bad key", request_id: "r1" } });
      }
      const unknown = Object.keys(body).filter((k) => !ALLOWED[req.url].includes(k));
      if (unknown.length) {
        return reply(400, { error: { code: "bad_json", message: `unknown ${unknown}`, request_id: "r2" } });
      }
      if (req.url === "/v1/compress") return reply(200, { messages: [{ role: "user", content: "short" }], stats: { ratio: 0.2 } });
      if (req.url === "/v1/compress/raw") return reply(200, { content: "short", stats: { ratio: 0.3 } });
      if (req.url === "/v1/retrieve") return reply(200, { found: false, hint: "regenerate it" });
      return reply(200, { content_type: "log", strategy: "log" });
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  url = `http://127.0.0.1:${server.address().port}`;
});

after(() => server.close());

test("compress sends only the fields given, in the server's spelling, with the key", async () => {
  seen.length = 0;
  const k = new Kompressor({ baseUrl: url, apiKey: "sk-test", tenant: "acme" });
  const out = await k.compress([{ role: "user", content: "x".repeat(100) }], { model: "m", frozenMessageCount: 0 });
  assert.equal(out.stats.ratio, 0.2);
  assert.equal(seen[0].path, "/v1/compress");
  assert.equal(seen[0].headers.authorization, "Bearer sk-test");
  assert.equal(seen[0].headers["x-kompressor-tenant"], "acme");
  assert.deepEqual(Object.keys(seen[0].body).sort(), ["frozen_message_count", "messages", "model"]);
});

test("every field each method can send is one the server accepts", async () => {
  seen.length = 0;
  const k = new Kompressor({ baseUrl: url });
  await k.compress([], { model: "m", provider: "p", frozenMessageCount: 1, context: "c", options: {}, requestId: "r" });
  await k.compressRaw("x", { contentType: "log", options: {}, requestId: "r", context: "c", sourceTool: "t", filePath: "f" });
  await k.retrieve("k_abc", { range: { lines: { start: 1, end: 2 } }, requestId: "r" });
  await k.detect("x", { role: "tool" });
  for (const { path, body } of seen) {
    assert.deepEqual(Object.keys(body).sort(), [...ALLOWED[path]].sort(), path);
  }
});

test("a server error carries status, code and request id", async () => {
  const k = new Kompressor({ baseUrl: url, apiKey: "wrong" });
  await assert.rejects(k.compress([]), (error) => {
    assert.ok(error instanceof KompressorError);
    assert.equal(error.status, 401);
    assert.equal(error.code, "unauthorized");
    assert.equal(error.requestId, "r1");
    return true;
  });
});

test("an unreachable server rejects, and fail-open returns the input", async () => {
  const k = new Kompressor({ baseUrl: "http://127.0.0.1:9", timeoutMs: 500 });
  await assert.rejects(k.compress([]), (error) => error instanceof KompressorError && error.status === 0);
  const messages = [{ role: "user", content: "keep me" }];
  assert.equal(await k.compressOrPassthrough(messages), messages);
  assert.equal(await k.health(), false);
});

test("the other calls, health, and a key that never serializes", async () => {
  const k = new Kompressor({ baseUrl: url, apiKey: "sk-secret" });
  assert.equal((await k.compressRaw("x".repeat(50))).content, "short");
  assert.equal((await k.retrieve("k_abc")).found, false);
  assert.equal((await k.detect("x")).strategy, "log");
  assert.equal(await k.health(), true);
  assert.ok(!JSON.stringify(k).includes("sk-secret"));
});

test("the request deadline covers a stalled response body", async () => {
  const k = new Kompressor({ timeoutMs: 20, fetch: async (_url, { signal }) => ({
    ok: true, status: 200,
    text: () => new Promise((_resolve, reject) => {
      signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
    }),
  }) });
  let timer;
  try {
    await assert.rejects(Promise.race([
      k.compress([]),
      new Promise((_resolve, reject) => { timer = setTimeout(() => reject(new Error("body deadline missing")), 500); }),
    ]), (error) => error instanceof KompressorError);
  } finally { clearTimeout(timer); }
});

test("malformed successful responses preserve the input in fail-open mode", async () => {
  const messages = [{ role: "user", content: "keep me" }];
  for (const body of ["<html>bad gateway</html>", "null", "[]", "{}", '{"messages":null}']) {
    const k = new Kompressor({ fetch: async () => new Response(body) });
    assert.equal(await k.compressOrPassthrough(messages), messages, body);
  }
});
