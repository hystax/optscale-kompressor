/**
 * A thin client for the OptScale Kompressor HTTP API.
 *
 * `fetch` only: Node 18+, Deno, Bun and browsers, no dependencies. Every
 * method is one request to `/v1/...` and resolves to the JSON the server
 * sent; request field names are the server's own (snake_case), because its
 * request structs refuse unknown fields.
 *
 *     import { Kompressor } from "@optscale/kompressor-client";
 *     const k = new Kompressor({ baseUrl: "http://127.0.0.1:8817" });
 *     const { messages, stats } = await k.compress(messages, { model: "claude-opus-5-5" });
 *
 * See docs/design/HEADROOM-PORT.md, Tier 6.
 */

export type Json = Record<string, unknown>;

export const DEFAULT_BASE_URL = "http://127.0.0.1:8817";

export interface KompressorOptions {
  /** Defaults to `$KOMPRESSOR_URL` where `process` exists, else 127.0.0.1:8817. */
  baseUrl?: string;
  /** Sent as `Authorization: Bearer`. Defaults to `$KOMPRESSOR_API_KEY`. */
  apiKey?: string;
  /** Milliseconds before a request is abandoned. */
  timeoutMs?: number;
  /** Sent as `x-kompressor-tenant`, for a key allowed to name one. */
  tenant?: string;
  /** A `fetch` of your own (tests, proxies, instrumented clients). */
  fetch?: typeof fetch;
}

export interface CompressOptions {
  model?: string;
  provider?: string;
  frozenMessageCount?: number;
  context?: string;
  options?: Json;
  requestId?: string;
}

export interface CompressRawOptions {
  contentType?: string;
  options?: Json;
  requestId?: string;
  context?: string;
  sourceTool?: string;
  filePath?: string;
}

export interface CompressResult {
  messages: Json[];
  stats: Json;
}

export interface CompressRawResult {
  content: string;
  stats: Json;
}

/** The server answered with an error, or could not be reached (`status` 0). */
export class KompressorError extends Error {
  constructor(
    message: string,
    readonly status: number = 0,
    readonly code?: string,
    readonly requestId?: string,
  ) {
    super(message);
    this.name = "KompressorError";
  }
}

function env(name: string): string | undefined {
  const proc = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  return proc?.env?.[name] || undefined;
}

/** Drop the keys whose value is undefined, so only what was given is sent. */
function defined(fields: Record<string, unknown>): Json {
  const out: Json = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}

export class Kompressor {
  readonly baseUrl: string;
  private readonly apiKey?: string;
  private readonly timeoutMs: number;
  private readonly tenant?: string;
  private readonly fetchImpl: typeof fetch;

  constructor(options: KompressorOptions = {}) {
    this.baseUrl = (options.baseUrl ?? env("KOMPRESSOR_URL") ?? DEFAULT_BASE_URL).replace(/\/+$/, "");
    this.apiKey = options.apiKey ?? env("KOMPRESSOR_API_KEY");
    this.timeoutMs = options.timeoutMs ?? 30_000;
    this.tenant = options.tenant;
    this.fetchImpl = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** Never prints the key. */
  toJSON(): Json {
    return { baseUrl: this.baseUrl, apiKey: this.apiKey ? "***" : undefined };
  }

  /** Compress a conversation in the caller's own message shape. */
  compress(messages: Json[], options: CompressOptions = {}): Promise<CompressResult> {
    return this.post<CompressResult>("/v1/compress", defined({
      messages,
      model: options.model,
      provider: options.provider,
      frozen_message_count: options.frozenMessageCount,
      context: options.context,
      options: options.options,
      request_id: options.requestId,
    }));
  }

  /** `compress` that never rejects: on any error the input comes back. */
  async compressOrPassthrough(messages: Json[], options: CompressOptions = {}): Promise<Json[]> {
    try {
      const result = (await this.compress(messages, options)).messages;
      return Array.isArray(result) ? result : messages;
    } catch {
      return messages;
    }
  }

  /** Compress one payload. */
  compressRaw(content: string, options: CompressRawOptions = {}): Promise<CompressRawResult> {
    return this.post<CompressRawResult>("/v1/compress/raw", defined({
      content,
      content_type: options.contentType,
      options: options.options,
      request_id: options.requestId,
      context: options.context,
      source_tool: options.sourceTool,
      file_path: options.filePath,
    }));
  }

  /** Fetch back what a marker's handle elided. A miss resolves to `{found: false, hint}`. */
  retrieve(ref: string, options: { range?: Json; requestId?: string } = {}): Promise<Json> {
    return this.post("/v1/retrieve", defined({ ref, range: options.range, request_id: options.requestId }));
  }

  /** What the engine would decide about a payload, without compressing it. */
  detect(content: string, options: { role?: string } = {}): Promise<Json> {
    return this.post("/v1/detect", defined({ content, role: options.role }));
  }

  /** Whether the server answers `/healthz`. */
  async health(): Promise<boolean> {
    try {
      const body = await this.request<Json>("GET", "/healthz");
      return body.status === "ok";
    } catch {
      return false;
    }
  }

  private post<T = Json>(path: string, body: Json): Promise<T> {
    return this.request<T>("POST", path, body);
  }

  private async request<T = Json>(method: string, path: string, body?: Json): Promise<T> {
    const headers: Record<string, string> = { accept: "application/json" };
    if (body !== undefined) headers["content-type"] = "application/json";
    if (this.apiKey) headers.authorization = `Bearer ${this.apiKey}`;
    if (this.tenant) headers["x-kompressor-tenant"] = this.tenant;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    let response: Response;
    let text: string;
    try {
      response = await this.fetchImpl(this.baseUrl + path, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: controller.signal,
      });
      // The deadline includes reading the body, not only receiving headers.
      text = await response.text();
    } catch (error) {
      throw new KompressorError(`kompressor at ${this.baseUrl} could not be reached: ${String(error)}`);
    } finally {
      clearTimeout(timer);
    }
    let parsed: Json = {};
    try {
      const value: unknown = JSON.parse(text);
      if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("expected an object");
      parsed = value as Json;
    } catch {
      if (response.ok) throw new KompressorError("kompressor returned invalid JSON", response.status);
    }
    if (!response.ok) {
      const envelope = (parsed.error ?? {}) as { message?: string; code?: string; request_id?: string };
      throw new KompressorError(
        envelope.message ?? `HTTP ${response.status}`,
        response.status,
        envelope.code,
        envelope.request_id,
      );
    }
    return parsed as T;
  }
}
