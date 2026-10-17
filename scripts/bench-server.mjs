import http from "node:http";
import { createHash } from "node:crypto";

const port = Number(process.env.PORT || 8787);
const CHUNK = 64 * 1024;
let live = 0;
let peak = 0;

const byteAt = (i) => (i * 2654435761) >>> 24;

function fill(buf, offset) {
  for (let i = 0; i < buf.length; i++) buf[i] = byteAt(offset + i);
  return buf;
}

function sha256(size) {
  const h = createHash("sha256");
  const buf = Buffer.alloc(1 << 20);
  for (let o = 0; o < size; o += buf.length) {
    const n = Math.min(buf.length, size - o);
    h.update(fill(buf.subarray(0, n), o));
  }
  return h.digest("hex");
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://localhost:${port}`);
  const m = url.pathname.match(/^\/file\/(\d+)(?:\/(.+))?$/);
  if (url.pathname === "/stats") {
    res.end(JSON.stringify({ live, peak }));
    peak = live;
    return;
  }
  if (url.pathname.startsWith("/sha/")) {
    res.end(sha256(Number(url.pathname.slice(5))));
    return;
  }
  if (!m) {
    res.writeHead(200, { "content-type": "text/html" });
    res.end(`<html><body><a href="/file/10485760/a.bin">a</a> <a href="/file/5242880/b.zip">b</a> <a href="/about">x</a></body></html>`);
    return;
  }
  const size = Number(m[1]);
  const rate = Number(url.searchParams.get("rate") || 2) * 1024 * 1024;
  const maxconn = Number(url.searchParams.get("maxconn") || 0);
  const noRange = url.searchParams.has("norange");
  const etag = url.searchParams.has("etagchange") ? `"${Date.now()}"` : `"v1-${size}"`;
  if (maxconn && live >= maxconn) {
    res.writeHead(503, { "retry-after": "2" });
    res.end();
    return;
  }
  let start = 0;
  let end = size - 1;
  const range = !noRange && req.headers.range && req.headers.range.match(/bytes=(\d+)-(\d*)/);
  const headers = {
    "content-type": "application/octet-stream",
    etag,
    "content-disposition": `attachment; filename="${m[2] || `bench-${size}.bin`}"`,
  };
  if (range) {
    start = Number(range[1]);
    end = range[2] ? Math.min(Number(range[2]), size - 1) : size - 1;
    if (start >= size) {
      res.writeHead(416, { "content-range": `bytes */${size}` });
      res.end();
      return;
    }
    res.writeHead(206, { ...headers, "accept-ranges": "bytes", "content-range": `bytes ${start}-${end}/${size}`, "content-length": end - start + 1 });
  } else {
    res.writeHead(200, { ...headers, "content-length": size });
  }
  if (req.method === "HEAD") return res.end();
  live++;
  peak = Math.max(peak, live);
  let offset = start;
  let closed = false;
  const done = () => {
    if (!closed) {
      closed = true;
      live--;
    }
  };
  res.on("close", done);
  const perTick = Math.max(1, Math.round(rate / 20));
  const pump = () => {
    if (closed) return;
    let budget = perTick;
    while (budget > 0 && offset <= end) {
      const n = Math.min(CHUNK, budget, end - offset + 1);
      const ok = res.write(fill(Buffer.allocUnsafe(n), offset));
      offset += n;
      budget -= n;
      if (!ok) break;
    }
    if (offset > end) {
      res.end();
      done();
      return;
    }
    setTimeout(pump, 50);
  };
  pump();
});

server.listen(port, () => console.log(`bench server on http://localhost:${port}`));
