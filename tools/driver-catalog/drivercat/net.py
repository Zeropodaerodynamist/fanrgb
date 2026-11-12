"""Polite HTTPS GET for the adapters (stdlib urllib only).

- An honest User-Agent that names the project; no browser impersonation, no cookies, no JavaScript.
- https only; redirects only to hosts on the fetch allow-list; at most 3 hops.
- Timeouts, a body size cap, gzip, and a few retries with backoff for 429/5xx/timeouts (honouring a
  short Retry-After). 403/404 are not retried: a vendor that blocks bots is reported, not hammered.
- At least MIN_INTERVAL seconds between two requests to the same host.
"""

import http.client
import socket
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib

USER_AGENT = "FanRGB-driver-catalog/1.0 (+https://github.com/Zeropodaerodynamist/fanrgb; weekly version check, 1 request per page)"
MAX_BODY = 5 * 1024 * 1024
TIMEOUT = 25
RETRIES = 2
MIN_INTERVAL = 3.0
MAX_RETRY_AFTER = 30


class FetchError(Exception):
    """A page could not be fetched (the message is safe to put in an issue)."""


class _Redirects(urllib.request.HTTPRedirectHandler):
    max_redirections = 3

    def __init__(self, allowed_hosts):
        super().__init__()
        self.allowed = set(allowed_hosts)

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        u = urllib.parse.urlsplit(newurl)
        if u.scheme != "https" or (u.hostname or "") not in self.allowed or u.port not in (None, 443):
            raise FetchError(f"refusing redirect to {u.scheme}://{u.hostname}{u.path}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


class Http:
    """One fetcher per run. `sleep` and `opener` are injectable for tests."""

    def __init__(self, allowed_hosts, sleep=time.sleep, clock=time.monotonic, opener=None, retries=RETRIES):
        self.retries = retries
        self.allowed = set(allowed_hosts)
        self.sleep = sleep
        self.clock = clock
        self.last = {}
        self.requests = 0
        self.opener = opener or urllib.request.build_opener(_Redirects(self.allowed))

    def _wait_turn(self, host):
        t = self.last.get(host)
        if t is not None:
            gap = MIN_INTERVAL - (self.clock() - t)
            if gap > 0:
                self.sleep(gap)
        self.last[host] = self.clock()

    def get(self, url, accept="text/html,application/xhtml+xml;q=0.9,*/*;q=0.5"):
        u = urllib.parse.urlsplit(url)
        host = u.hostname or ""
        if u.scheme != "https" or host not in self.allowed:
            raise FetchError(f"not an allow-listed https URL: {u.scheme}://{host}")
        last_error = None
        for attempt in range(self.retries + 1):
            self._wait_turn(host)
            self.requests += 1
            req = urllib.request.Request(url, headers={
                "User-Agent": USER_AGENT,
                "Accept": accept,
                "Accept-Language": "en-US,en;q=0.8",
                "Accept-Encoding": "gzip",
            })
            retry_after = None
            try:
                with self.opener.open(req, timeout=TIMEOUT) as r:
                    body = r.read(MAX_BODY + 1)
                    if len(body) > MAX_BODY:
                        raise FetchError(f"{host}: response larger than {MAX_BODY} bytes")
                    if (r.headers.get("Content-Encoding") or "").lower() == "gzip":
                        body = _gunzip(body, host)
                    charset = r.headers.get_content_charset() or "utf-8"
                    return body.decode(charset, errors="replace")
            except FetchError:
                raise
            except urllib.error.HTTPError as e:
                if e.code not in (429, 500, 502, 503, 504):
                    raise FetchError(f"{host}: HTTP {e.code}") from None
                last_error = f"{host}: HTTP {e.code}"
                retry_after = _retry_after(e.headers.get("Retry-After") if e.headers else None)
            except (urllib.error.URLError, socket.timeout, TimeoutError, ConnectionError, http.client.HTTPException, OSError) as e:
                reason = getattr(e, "reason", e)
                last_error = f"{host}: {type(reason).__name__}: {str(reason)[:120]}"
            if attempt < self.retries:
                self.sleep(retry_after if retry_after is not None else 5 * (attempt + 1) ** 2)
        raise FetchError(f"{last_error} (after {self.retries + 1} attempts)")


def _gunzip(data, host):
    """Inflates a gzip body, never producing more than MAX_BODY bytes (a small response can otherwise
    inflate to gigabytes before any size check)."""
    d = zlib.decompressobj(wbits=31)
    try:
        out = d.decompress(data, MAX_BODY + 1)
    except zlib.error as e:
        raise FetchError(f"{host}: invalid gzip body ({str(e)[:80]})") from None
    if len(out) > MAX_BODY or d.unconsumed_tail:
        raise FetchError(f"{host}: response larger than {MAX_BODY} bytes")
    if not d.eof:
        raise FetchError(f"{host}: truncated gzip body")
    return out


def _retry_after(v):
    try:
        s = int(v)
    except (TypeError, ValueError):
        return None
    return max(0, min(s, MAX_RETRY_AFTER))
