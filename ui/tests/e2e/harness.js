// Test support for browser tests: a local file server the daemon downloads
// from, a local stand-in for Real-Debrid's API, and a `wayfetchd`
// subprocess with browser sign-in over loopback.

import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/** The daemon under test: `WAYFETCHD`, or the workspace's debug build. */
export const WAYFETCHD =
  process.env.WAYFETCHD ??
  resolve(dirname(fileURLToPath(import.meta.url)), '../../../target/debug/wayfetchd');

export const MIB = 1024 * 1024;

/**
 * The byte at `offset` of every fixture file.
 * @param {number} offset
 */
function byteAt(offset) {
  return (offset * 7 + 3) & 0xff;
}

/**
 * A fixture file's bytes from `start` to `end`.
 * @param {number} start
 * @param {number} end
 */
export function content(start, end) {
  const bytes = Buffer.allocUnsafe(end - start);
  for (let i = 0; i < bytes.length; i += 1) bytes[i] = byteAt(start + i);
  return bytes;
}

/**
 * @typedef {{ size: number, stallAt: number | null, etag: string, filename: string | null, waiters: (() => void)[] }} FixtureFile
 */

/**
 * A plain-HTTP server on 127.0.0.1 serving generated files with ranges
 * and a strong validator, and web pages. A file can stall at an offset
 * until released, so a test can act while its download is in progress.
 * `HEAD`, which link checks send, answers a file's headers and is logged
 * apart from downloads.
 */
export class Fixture {
  constructor() {
    /** @type {Map<string, FixtureFile>} */
    this.files = new Map();
    /** Names of the files requested so far, in order. @type {string[]} */
    this.requested = [];
    /** Each request's Range header, or null, in order. @type {(string | null)[]} */
    this.ranges = [];
    /** Names of the files checked with `HEAD` so far, in order. @type {string[]} */
    this.checked = [];
    /** Web pages by name. @type {Map<string, string>} */
    this.pages = new Map();
    this.server = createServer((request, response) => void this.serve(request, response));
  }

  async start() {
    this.server.listen(0, '127.0.0.1');
    await once(this.server, 'listening');
    const address = /** @type {import('node:net').AddressInfo} */ (this.server.address());
    this.base = `http://127.0.0.1:${address.port}`;
    return this;
  }

  /**
   * Serves `name` with `size` bytes, stalling at `stallAt` if given.
   * @param {string} name
   * @param {number} size
   * @param {number | null} [stallAt]
   * @param {string | null} [filename]
   */
  put(name, size, stallAt = null, filename = null) {
    this.files.set(name, { size, stallAt, filename, etag: '"v1"', waiters: [] });
    return `${this.base}/files/${encodeURIComponent(name)}`;
  }

  /**
   * Serves `html` as web page `name`.
   * @param {string} name
   * @param {string} html
   */
  page(name, html) {
    this.pages.set(name, html);
    return `${this.base}/pages/${encodeURIComponent(name)}`;
  }

  /** @param {string} name */
  release(name) {
    const file = /** @type {FixtureFile} */ (this.files.get(name));
    file.stallAt = null;
    for (const wake of file.waiters.splice(0)) wake();
  }

  /**
   * Gives `name` a new validator, as a file replaced on the server would
   * have, so a saved part of the old one cannot be continued.
   * @param {string} name
   */
  replace(name) {
    const file = /** @type {FixtureFile} */ (this.files.get(name));
    file.etag = '"v2"';
  }

  /**
   * @param {import('node:http').IncomingMessage} request
   * @param {import('node:http').ServerResponse} response
   */
  async serve(request, response) {
    // A query, like a CDN's signed token, does not name the file.
    const path = new URL(request.url ?? '/', this.base).pathname;
    const page = path.startsWith('/pages/') ? this.pages.get(decodeURIComponent(path.slice('/pages/'.length))) : undefined;
    if (page !== undefined) {
      const body = Buffer.from(page);
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Content-Length': body.length });
      response.end(request.method === 'HEAD' ? undefined : body);
      return;
    }
    const name = decodeURIComponent(path.replace(/^\/files\//, ''));
    const file = this.files.get(name);
    if (request.method === 'HEAD') {
      this.checked.push(name);
    } else {
      this.requested.push(name);
      this.ranges.push(request.headers.range ?? null);
    }
    if (!file || (request.method !== 'GET' && request.method !== 'HEAD')) {
      response.writeHead(404).end();
      return;
    }
    let start = 0;
    const range = /^bytes=(\d+)-$/.exec(request.headers.range ?? '');
    const ifRange = request.headers['if-range'];
    const headers = {
      ETag: file.etag,
      'Accept-Ranges': 'bytes',
      'Content-Type': 'application/octet-stream',
      ...(file.filename === null ? {} : { 'Content-Disposition': `attachment; filename*=UTF-8''${encodeURIComponent(file.filename)}` }),
    };
    if (range && (ifRange === undefined || ifRange === file.etag)) {
      start = Number(range[1]);
      response.writeHead(206, {
        ...headers,
        'Content-Length': file.size - start,
        'Content-Range': `bytes ${start}-${file.size - 1}/${file.size}`,
      });
    } else {
      response.writeHead(200, { ...headers, 'Content-Length': file.size });
    }
    if (request.method === 'HEAD') {
      response.end();
      return;
    }
    // The headers go out even if the body stalls at once.
    response.flushHeaders();
    const chunk = 256 * 1024;
    for (let offset = start; offset < file.size && !response.destroyed; ) {
      if (file.stallAt !== null && offset >= file.stallAt) {
        await new Promise((wake) => {
          file.waiters.push(() => wake(undefined));
          response.once('close', () => wake(undefined));
        });
        continue;
      }
      const end = Math.min(file.size, offset + chunk, file.stallAt ?? Infinity);
      if (!response.write(content(offset, end))) await once(response, 'drain').catch(() => {});
      offset = end;
    }
    response.end();
  }

  async stop() {
    this.server.closeAllConnections();
    this.server.close();
  }
}

/** Values the Real-Debrid stand-in hands out that grant access. */
export const RD_SECRETS = Object.freeze({
  deviceCode: 'synthetic-device-code',
  clientSecret: 'synthetic-user-client-secret',
  access: 'synthetic-access-',
  refresh: 'synthetic-refresh-',
});
export const RD_USER_CODE = 'ABCD-1234';
/** What every link the stand-in generates carries, which grants the download. */
export const RD_SIGNATURE = 'synthetic-download-signature';

/**
 * A plain-HTTP stand-in for Real-Debrid's API on 127.0.0.1, following its
 * documented device flow, `GET /rest/1.0/user`, `POST
 * /rest/1.0/unrestrict/check` (no sign-in), `POST
 * /rest/1.0/unrestrict/link`, and `GET /rest/1.0/downloads`, and serving
 * the links it generates. A sign-in is approved on the second poll; every
 * value is synthetic.
 */
export class RealDebridFixture {
  constructor() {
    this.polls = 0;
    this.issued = 0;
    /** @type {string | null} */
    this.access = null;
    /** @type {string | null} */
    this.refresh = null;
    this.username = 'synthetic-user';
    /** Requests by path, in order. @type {string[]} */
    this.requested = [];
    /** Links generated, by the original link asked for. @type {{ link: string, path: string }[]} */
    this.generated = [];
    /** The file each generated link serves. */
    this.size = 3 * MIB + 7;
    this.filename = 'Through the Provider.bin';
    /** Unrestrict requests whose answer is lost: the connection drops before it. */
    this.lose = 0;
    /** Optional status returned by unrestrict, without a provider error code. @type {number | null} */
    this.refuseUnrestrict = null;
    /** Authorization headers that reached generated links. @type {(string | undefined)[]} */
    this.downloadAuth = [];
    this.server = createServer((request, response) => void this.serve(request, response));
  }

  async start() {
    this.server.listen(0, '127.0.0.1');
    await once(this.server, 'listening');
    const address = /** @type {import('node:net').AddressInfo} */ (this.server.address());
    this.base = `http://127.0.0.1:${address.port}/`;
    return this;
  }

  /**
   * @param {import('node:http').IncomingMessage} request
   * @param {import('node:http').ServerResponse} response
   */
  async serve(request, response) {
    const url = new URL(request.url ?? '/', this.base);
    this.requested.push(url.pathname);
    let body = '';
    for await (const chunk of request) body += chunk;
    const form = new URLSearchParams(body);
    const origin = this.base?.replace(/\/$/, '');
    /** @param {number} status @param {unknown} data */
    const answer = (status, data) => {
      response.writeHead(status, { 'Content-Type': 'application/json' }).end(JSON.stringify(data));
    };
    switch (url.pathname) {
      case '/rest/1.0/hosts/domains':
        answer(200, ['hoster.example']);
        return;
      case '/rest/1.0/unrestrict/check':
        // Named as the link's path, so collected rows keep their names.
        answer(200, {
          host: 'hoster.example',
          link: form.get('link'),
          filename: (form.get('link') ?? '').split('/').pop(),
          filesize: this.size,
          supported: 1,
        });
        return;
      case '/oauth/v2/device/code':
        this.polls = 0;
        answer(200, {
          device_code: RD_SECRETS.deviceCode,
          user_code: RD_USER_CODE,
          interval: 1,
          expires_in: 600,
          verification_url: `${origin}/device`,
        });
        return;
      case '/oauth/v2/device/credentials':
        this.polls += 1;
        if (url.searchParams.get('code') !== RD_SECRETS.deviceCode || this.polls < 2) {
          answer(403, { error: 'authorization_pending', error_code: 1 });
          return;
        }
        answer(200, { client_id: 'synthetic-user-client-id', client_secret: RD_SECRETS.clientSecret });
        return;
      case '/oauth/v2/token': {
        const code = form.get('code');
        const ok =
          form.get('client_secret') === RD_SECRETS.clientSecret &&
          (code === RD_SECRETS.deviceCode || (this.refresh !== null && code === this.refresh));
        if (!ok) {
          answer(400, { error: 'invalid_grant', error_code: 9 });
          return;
        }
        this.issued += 1;
        this.access = `${RD_SECRETS.access}${this.issued}`;
        this.refresh = `${RD_SECRETS.refresh}${this.issued}`;
        answer(200, { access_token: this.access, expires_in: 3600, token_type: 'Bearer', refresh_token: this.refresh });
        return;
      }
      case '/rest/1.0/unrestrict/link':
      case '/rest/1.0/downloads': {
        if (this.access === null || request.headers.authorization !== `Bearer ${this.access}`) {
          answer(401, { error: 'bad_token', error_code: 8 });
          return;
        }
        const download = (/** @type {number} */ n) => ({
          id: `SYNTH${n}`,
          filename: this.filename,
          mimeType: 'application/octet-stream',
          filesize: this.size,
          link: this.generated[n - 1].link,
          host: 'hoster.example',
          chunks: 16,
          download: `${origin}${this.generated[n - 1].path}?sig=${RD_SIGNATURE}-${n}`,
          generated: new Date().toISOString(),
        });
        if (url.pathname === '/rest/1.0/downloads') {
          if (this.generated.length === 0) {
            response.writeHead(204).end();
            return;
          }
          answer(200, this.generated.map((_, i) => download(i + 1)).reverse());
          return;
        }
        if (this.refuseUnrestrict !== null) {
          answer(this.refuseUnrestrict, { error: 'synthetic-private-provider-detail' });
          return;
        }
        if (this.lose > 0) {
          this.lose -= 1;
          request.socket.destroy();
          return;
        }
        const link = form.get('link') ?? '';
        this.generated.push({ link, path: `/d/${this.generated.length + 1}/${encodeURIComponent(this.filename)}` });
        answer(200, download(this.generated.length));
        return;
      }
      case '/rest/1.0/user':
        if (this.access === null || request.headers.authorization !== `Bearer ${this.access}`) {
          answer(401, { error: 'bad_token', error_code: 8 });
          return;
        }
        answer(200, { id: 4242, username: this.username, type: 'premium', premium: 30 * 86400 });
        return;
      default:
        if (this.generated.some((g) => g.path === url.pathname)) {
          this.downloadAuth.push(request.headers.authorization);
          response.writeHead(200, { 'Content-Type': 'application/octet-stream', 'Content-Length': this.size });
          response.end(content(0, this.size));
          return;
        }
        answer(404, { error: 'unknown_ressource', error_code: 7 });
    }
  }

  async stop() {
    this.server.closeAllConnections();
    this.server.close();
  }
}

/** A free TCP port on 127.0.0.1. */
export async function freePort() {
  const server = createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const { port } = /** @type {import('node:net').AddressInfo} */ (server.address());
  server.close();
  await once(server, 'close');
  return port;
}

/**
 * A `wayfetchd` with its own state directory, download root, a second
 * permitted folder, token file (with the credential key beside it), and auth directory,
 * serving browsers on a fixed loopback port across restarts, with
 * Real-Debrid's API at `realDebrid`, GitHub's API for releases at
 * `updates`, and the plugins at `plugins` if given.
 */
export class Daemon {
  /**
   * @param {number} port
   * @param {{ realDebrid?: string, updates?: string, plugins?: { ytDlp?: string, ffmpeg?: string, sevenZip?: string } }} [options]
   */
  constructor(port, options = {}) {
    this.port = port;
    this.realDebrid = options.realDebrid;
    this.updates = options.updates;
    this.plugins = options.plugins;
    this.origin = `http://127.0.0.1:${port}`;
    /** @type {import('node:child_process').ChildProcess | null} */
    this.child = null;
    this.log = '';
  }

  async init() {
    this.dir = await mkdtemp(join(tmpdir(), 'wayfetch-ui-'));
    this.state = join(this.dir, 'state');
    this.root = join(this.dir, 'downloads');
    this.archive = join(this.dir, 'archive');
    this.auth = join(this.dir, 'auth');
    for (const folder of [this.root, this.archive]) {
      await mkdir(folder);
      const init = spawnSync(WAYFETCHD, ['init-root', '--state-dir', this.state, folder]);
      if (init.status !== 0) throw new Error(`init-root failed: ${init.stderr}`);
    }
    return this;
  }

  async start() {
    const config = join(/** @type {string} */ (this.dir), 'wayfetch.toml');
    if (this.plugins) {
      const { ytDlp, ffmpeg, sevenZip } = this.plugins;
      const paths = Object.entries({ yt_dlp: ytDlp, ffmpeg, seven_zip: sevenZip })
        .filter(([, path]) => path !== undefined)
        .map(([key, path]) => `${key} = ${JSON.stringify(path)}\n`);
      await writeFile(config, `schema_version = 1\n[plugins]\n${paths.join('')}`);
    }
    const child = spawn(
      WAYFETCHD,
      [
        'run',
        '--state-dir', /** @type {string} */ (this.state),
        '--download-root', /** @type {string} */ (this.root),
        '--root', /** @type {string} */ (this.archive),
        '--token-file', join(/** @type {string} */ (this.dir), 'api-tokens'),
        '--listen', `127.0.0.1:${this.port}`,
        '--auth-dir', /** @type {string} */ (this.auth),
        '--dev-insecure-loopback',
        '--allow-network', '127.0.0.1/32',
        ...(this.realDebrid ? ['--provider-endpoint', `real-debrid=${this.realDebrid}`] : []),
        // Nowhere by default, so an automatic check never reaches GitHub.
        '--updates-endpoint',
        this.updates ?? 'http://127.0.0.1:9/',
        ...(this.plugins ? ['--config', config] : []),
        '--json',
      ],
      { stdio: ['ignore', 'pipe', 'pipe'] },
    );
    this.child = child;
    child.stdout?.on('data', (data) => (this.log += data));
    child.stderr?.on('data', (data) => (this.log += data));
    const deadline = Date.now() + 30_000;
    for (;;) {
      if (child.exitCode !== null) throw new Error(`wayfetchd exited:\n${this.log}`);
      const ready = await fetch(`${this.origin}/v1/ready`).catch(() => null);
      if (ready?.ok) return;
      if (Date.now() > deadline) throw new Error(`wayfetchd not ready:\n${this.log}`);
      await new Promise((wait) => setTimeout(wait, 100));
    }
  }

  /** Stops the daemon gracefully and waits for it to exit. */
  async stop() {
    const child = this.child;
    if (!child || child.exitCode !== null) return;
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await exited;
    this.child = null;
  }

  /** The current setup secret. */
  setupSecret() {
    return readFile(join(/** @type {string} */ (this.auth), 'setup-secret'), 'utf8');
  }

  /**
   * @param {string} name
   * @param {string} [folder] the download root unless given
   */
  published(name, folder = this.root) {
    return readFile(join(/** @type {string} */ (folder), name));
  }

  async remove() {
    await this.stop();
    if (this.dir) await rm(this.dir, { recursive: true, force: true });
  }
}
