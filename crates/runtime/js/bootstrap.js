// Worker のグローバルスコープに Web API を生やすブートストラップ。
// Context 作成直後に classic script として評価される。
// Rust 側が globalThis.__ops にネイティブ関数を注入済みで、評価後に
// __runtime のヘルパを Global<Function> として取り出してから両方 delete する。
(({ print, timer, fetch: opFetch, encodeUtf8, decodeUtf8 }) => {
  const normalizeName = (name) => String(name).toLowerCase();

  class Headers {
    #map = new Map();
    constructor(init) {
      if (init === undefined || init === null) return;
      if (init instanceof Headers || Array.isArray(init)) {
        for (const [k, v] of init) this.append(k, v);
      } else if (typeof init === "object") {
        for (const k of Object.keys(init)) this.append(k, init[k]);
      }
    }
    append(name, value) {
      name = normalizeName(name);
      value = String(value);
      const cur = this.#map.get(name);
      this.#map.set(name, cur === undefined ? value : cur + ", " + value);
    }
    set(name, value) {
      this.#map.set(normalizeName(name), String(value));
    }
    get(name) {
      return this.#map.get(normalizeName(name)) ?? null;
    }
    has(name) {
      return this.#map.has(normalizeName(name));
    }
    delete(name) {
      this.#map.delete(normalizeName(name));
    }
    *[Symbol.iterator]() {
      yield* this.#map.entries();
    }
    entries() {
      return this.#map.entries();
    }
    keys() {
      return this.#map.keys();
    }
    values() {
      return this.#map.values();
    }
    forEach(cb, thisArg) {
      for (const [k, v] of this) cb.call(thisArg, v, k, this);
    }
  }

  const toBodyBytes = (body) => {
    if (body === undefined || body === null) return null;
    if (typeof body === "string") return new Uint8Array(encodeUtf8(body));
    if (body instanceof ArrayBuffer) return new Uint8Array(body.slice(0));
    if (ArrayBuffer.isView(body)) {
      return new Uint8Array(
        body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
      );
    }
    throw new TypeError("unsupported body type");
  };

  class Body {
    #bytes;
    #used = false;
    constructor(bytes) {
      this.#bytes = bytes;
    }
    get bodyUsed() {
      return this.#used;
    }
    #consume() {
      if (this.#used) throw new TypeError("body already used");
      this.#used = true;
      return this.#bytes ?? new Uint8Array(0);
    }
    async arrayBuffer() {
      const b = this.#consume();
      return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
    }
    async text() {
      return decodeUtf8(await this.arrayBuffer());
    }
    async json() {
      return JSON.parse(await this.text());
    }
    // ランタイム内部用（fetch のシリアライズで使う）
    _bodyBytes() {
      return this.#bytes ?? null;
    }
  }

  class Request extends Body {
    #url;
    #method;
    #headers;
    constructor(input, init = {}) {
      const isReq = typeof input === "object" && input !== null;
      super(
        init.body !== undefined && init.body !== null
          ? toBodyBytes(init.body)
          : isReq && input._bodyBytes
            ? input._bodyBytes()
            : null,
      );
      this.#url = isReq ? input.url : String(input);
      this.#method = String(init.method ?? (isReq ? input.method : "GET")).toUpperCase();
      this.#headers = new Headers(init.headers ?? (isReq ? input.headers : undefined));
    }
    get url() {
      return this.#url;
    }
    get method() {
      return this.#method;
    }
    get headers() {
      return this.#headers;
    }
  }

  class Response extends Body {
    #status;
    #statusText;
    #headers;
    constructor(body = null, init = {}) {
      super(toBodyBytes(body));
      this.#status = init.status ?? 200;
      this.#statusText = init.statusText ?? "";
      this.#headers = new Headers(init.headers);
    }
    get status() {
      return this.#status;
    }
    get statusText() {
      return this.#statusText;
    }
    get ok() {
      return this.#status >= 200 && this.#status < 300;
    }
    get headers() {
      return this.#headers;
    }
    static json(data, init = {}) {
      const resp = new Response(JSON.stringify(data), init);
      if (!resp.headers.has("content-type")) {
        resp.headers.set("content-type", "application/json");
      }
      return resp;
    }
  }

  class TextEncoder {
    encode(input = "") {
      return new Uint8Array(encodeUtf8(String(input)));
    }
  }

  class TextDecoder {
    decode(input) {
      return decodeUtf8(input);
    }
  }

  const inspect = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return v.stack ?? String(v);
    try {
      return JSON.stringify(v) ?? String(v);
    } catch {
      return String(v);
    }
  };
  const console = {
    log: (...args) => print(args.map(inspect).join(" ") + "\n"),
    info: (...args) => print(args.map(inspect).join(" ") + "\n"),
    warn: (...args) => print(args.map(inspect).join(" ") + "\n"),
    error: (...args) => print(args.map(inspect).join(" ") + "\n"),
  };

  // タイマー ID・cancel は未対応の最小実装
  const setTimeout = (cb, ms = 0, ...args) => {
    timer(Math.max(0, Number(ms))).then(() => cb(...args));
  };

  // サブリクエスト fetch。Rust 側 op に平坦な形で渡し、結果から Response を組む
  const fetch = async (input, init) => {
    const req = new Request(input, init);
    const bytes = req._bodyBytes();
    const bodyAb = bytes
      ? bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
      : undefined;
    const r = await opFetch(req.url, req.method, [...req.headers], bodyAb);
    return new Response(r.body.byteLength > 0 ? r.body : null, {
      status: r.status,
      headers: r.headers,
    });
  };

  // ランタイム内部用フック。Rust 側が Global<Function> として取り出した後 delete する。
  globalThis.__runtime = {
    buildRequest: (url, method, headersArr, bodyAb) =>
      new Request(url, { method, headers: headersArr, body: bodyAb }),
    serializeResponse: async (resp) => {
      if (!(resp instanceof Response)) {
        throw new TypeError("fetch handler must return a Response");
      }
      const body = resp.bodyUsed ? new ArrayBuffer(0) : await resp.arrayBuffer();
      return [resp.status, [...resp.headers], body];
    },
  };

  Object.assign(globalThis, {
    Headers,
    Request,
    Response,
    TextEncoder,
    TextDecoder,
    console,
    setTimeout,
    fetch,
  });
})(globalThis.__ops);
delete globalThis.__ops;
