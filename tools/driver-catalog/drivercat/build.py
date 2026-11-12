"""Builds the next catalog: adapter results + manual entries + the published catalog, with the same
validation rules the app applies (src/drivers/catalog/mod.rs) plus CI-only rules (no downgrade, stale
manual entries).

Rules:
- serial = published serial + 1; generated_at = now; expires = now + 21 days.
- An adapter that fails (fetch, parse or validation) keeps its previously published entries unchanged
  (checked_at is not refreshed) and is reported; it never blocks the other adapters.
- A version lower than the published one is refused (the previous entry is kept) unless the id is in
  allow_downgrade (workflow input or manual.json).
- catalog/manual.json entries replace automatic entries with the same id; entries checked more than 60
  days ago, and automatic results newer than a manual pin, are reported.
"""

import datetime
import re
import unicodedata

from . import schema as jsonschema
from . import versions
from .net import FetchError
from .adapters import AdapterError

KIND = "fanrgb-driver-catalog"
SCHEMA_VERSION = "1.0"
EXPIRES_DAYS = 21
MAX_VALIDITY_DAYS = 90
FUTURE_SLACK = datetime.timedelta(days=2)
MANUAL_MAX_AGE_DAYS = 60
_URL_PATH = re.compile(r"[A-Za-z0-9\-._~/?#&=+,;:]*")


def ts(dt):
    return dt.astimezone(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_ts(s):
    if not isinstance(s, str) or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", s):
        raise ValueError(f"not YYYY-MM-DDTHH:MM:SSZ: {s!r}")
    return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc)


def is_hidden(c):
    """The app's `drivers::is_hidden`: control, invisible and reordering characters."""
    u = ord(c)
    if unicodedata.category(c) == "Cc" or (u & 0xFFFE) == 0xFFFE:
        return True
    singles = {0x00AD, 0x034F, 0x061C, 0x06DD, 0x070F, 0x08E2, 0x3164, 0xFEFF, 0xFFA0, 0x110BD, 0x110CD}
    ranges = [(0x0600, 0x0605), (0x0890, 0x0891), (0x115F, 0x1160), (0x17B4, 0x17B5), (0x180B, 0x180F),
              (0x200B, 0x200F), (0x202A, 0x202E), (0x2060, 0x206F), (0xE000, 0xF8FF), (0xFDD0, 0xFDEF),
              (0xFE00, 0xFE0F), (0xFFF0, 0xFFFC), (0x13430, 0x1343F), (0x1BCA0, 0x1BCA3), (0x1D173, 0x1D17A),
              (0xE0000, 0xE0FFF), (0xF0000, 0x10FFFF)]
    return u in singles or any(a <= u <= b for a, b in ranges)


def text_ok(s, max_len=120):
    return (isinstance(s, str) and s.strip() != "" and len(s) <= max_len
            and not any(is_hidden(c) or (c.isspace() and c != " ") for c in s))


def url_ok(url, hosts):
    """The app's `drivers::catalog::url_allowed`."""
    if not isinstance(url, str) or len(url) > 300 or not url.startswith("https://"):
        return False
    rest = url[len("https://"):]
    cut = min([i for i in (rest.find("/"), rest.find("?"), rest.find("#")) if i >= 0] or [len(rest)])
    host, path = rest[:cut], rest[cut:]
    if host not in hosts or (path and not path.startswith("/")):
        return False
    return _URL_PATH.fullmatch(path) is not None


def _date_ok(s):
    try:
        datetime.date.fromisoformat(s)
        return re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", s) is not None
    except (TypeError, ValueError):
        return False


def entry_problems(e, hosts):
    """Semantic checks of one entry beyond the JSON schema (what the app refuses)."""
    p = []
    id = e.get("id", "?")
    scheme = e.get("version_scheme")
    if scheme not in versions.SCHEMES or not versions.is_valid(scheme, e.get("version")):
        p.append(f"{id}: version {str(e.get('version'))[:40]!r} is not a valid {scheme} version")
    for f in ("vendor", "component"):
        if not text_ok(e.get(f)):
            p.append(f"{id}: {f} is empty, too long or has control/invisible characters")
    if not url_ok(e.get("download_url"), hosts):
        p.append(f"{id}: download_url is not an allow-listed https vendor page")
    if e.get("release_notes_url") is not None and not url_ok(e["release_notes_url"], hosts):
        p.append(f"{id}: release_notes_url is not an allow-listed https vendor page")
    if e.get("release_date") is not None and not _date_ok(e["release_date"]):
        p.append(f"{id}: release_date is not a valid YYYY-MM-DD date")
    m = e.get("match") or {}
    for pat in m.get("hardware_ids", []):
        lit = len(pat) if not re.search(r"[*?]", pat) else re.search(r"[*?]", pat).start()
        if lit < 8:
            p.append(f"{id}: hardware-ID pattern needs 8 literal characters before a wildcard")
    for t in m.get("providers", []) + m.get("packages", []):
        if not text_ok(t):
            p.append(f"{id}: a provider/package value is invalid")
    if not (m.get("hardware_ids") or m.get("packages") or m.get("board")):
        p.append(f"{id}: match needs hardware_ids, packages or board")
    return p


def validate(catalog, schema, hosts, now, previous=None, allow_downgrade=()):
    """Everything the app checks, plus the serial and downgrade rules against the published catalog."""
    errs = jsonschema.errors(catalog, schema)
    if errs:
        return errs
    try:
        gen = parse_ts(catalog["generated_at"])
        exp = parse_ts(catalog["expires"])
    except ValueError as e:
        return [str(e)]
    if gen > now + FUTURE_SLACK:
        errs.append("generated_at is in the future")
    if not (gen < exp <= gen + datetime.timedelta(days=MAX_VALIDITY_DAYS)):
        errs.append("expires must be after generated_at and at most 90 days later")
    ids = [e["id"] for e in catalog["entries"]]
    if len(ids) != len(set(ids)):
        errs.append("duplicate entry ids")
    for e in catalog["entries"]:
        errs += entry_problems(e, hosts)
        if parse_ts(e["checked_at"]) > gen:
            errs.append(f"{e['id']}: checked_at is after generated_at")
    if previous is not None:
        if catalog["serial"] != previous["serial"] + 1:
            errs.append(f"serial {catalog['serial']} is not the published serial {previous['serial']} + 1")
        old = {e["id"]: e for e in previous["entries"]}
        for e in catalog["entries"]:
            o = old.get(e["id"])
            if o and o["version_scheme"] == e["version_scheme"] and e["id"] not in allow_downgrade \
                    and versions.compare_latest(e["version_scheme"], e["version"], o["version"]) == -1:
                errs.append(f"{e['id']}: {e['version']} is older than the published {o['version']} (downgrade)")
    elif catalog["serial"] != 1:
        errs.append("no published catalog: the serial must be 1")
    return errs


def run_adapters(adapters, http=None, fixtures=None, save=None):
    """Runs every adapter; one that fails never stops the others. {name: {"ok", "results"|"error"}}"""
    out = {}
    for a in adapters:
        try:
            if fixtures is not None:
                with open(f"{fixtures}/{a.fixture}", encoding="utf-8") as f:
                    html = f.read()
            else:
                html = http.get(a.url)
            if save is not None:
                with open(f"{save}/{a.name}.html", "w", encoding="utf-8") as f:
                    f.write(html)
            out[a.name] = {"ok": True, "results": a.parse(html)}
        except (FetchError, AdapterError) as e:
            out[a.name] = {"ok": False, "error": str(e)[:300]}
        except Exception as e:  # a parser bug must not block the other vendors
            out[a.name] = {"ok": False, "error": f"{type(e).__name__}: {str(e)[:200]}"}
    return out


def build(previous, manual, outcomes, adapters, now, hosts, allow_downgrade=()):
    """-> (catalog, report). `now` is an aware UTC datetime."""
    now = now.replace(microsecond=0)
    now_s = ts(now)
    prev = {e["id"]: e for e in (previous or {}).get("entries", [])}
    allow = set(allow_downgrade) | set(manual.get("allow_downgrade", []))
    report = {"serial": (previous["serial"] + 1) if previous else 1, "failures": [], "warnings": [], "changes": []}
    entries = {}

    def keep(id, why, adapter):
        if id in prev:
            entries[id] = prev[id]
        report["failures"].append({"adapter": adapter, "id": id, "error": why, "kept_previous": id in prev})

    for a in adapters:
        o = outcomes.get(a.name, {"ok": False, "error": "not run"})
        for static in a.entries():
            id = static["id"]
            if not o["ok"]:
                keep(id, o["error"], a.name)
                continue
            r = o["results"].get(id)
            if not r:
                keep(id, "the adapter returned no result for this entry", a.name)
                continue
            new = dict(static)
            new.update({
                "version": r["version"], "release_date": r.get("release_date"),
                "release_notes_url": r.get("release_notes_url"), "source": f"auto:{a.name}", "checked_at": now_s,
            })
            problems = entry_problems(new, hosts)
            if problems:
                keep(id, "; ".join(problems), a.name)
                continue
            old = prev.get(id)
            if old and old["version_scheme"] == new["version_scheme"]:
                c = versions.compare_latest(new["version_scheme"], new["version"], old["version"])
                if c == -1 and id not in allow:
                    keep(id, f"{new['version']} is older than the published {old['version']}: downgrade refused "
                             f"(add {id} to allow_downgrade if the vendor really pulled it)", a.name)
                    continue
                if c != 0:
                    report["changes"].append(f"{id}: {old['version']} -> {new['version']}")
            elif not old:
                report["changes"].append(f"{id}: new, {new['version']}")
            entries[id] = new

    for m in manual.get("entries", []):
        id = m.get("id", "?")
        auto = entries.get(id)
        if auto and auto["source"].startswith("auto:") and auto["version_scheme"] == m.get("version_scheme") \
                and versions.compare_latest(auto["version_scheme"], auto["version"], m.get("version")) == 1:
            report["warnings"].append(f"{id}: {auto['source']} found {auto['version']}, newer than the manual pin "
                                      f"{m.get('version')}; update or remove the manual entry")
        try:
            age = (now - parse_ts(m.get("checked_at"))).days
        except ValueError:
            age = None
        if age is not None and age > MANUAL_MAX_AGE_DAYS:
            report["warnings"].append(f"{id}: manual entry last checked {age} days ago (over {MANUAL_MAX_AGE_DAYS}); "
                                      "confirm it on the vendor page and update checked_at")
        old = prev.get(id)
        if old and old.get("version") != m.get("version"):
            report["changes"].append(f"{id}: {old.get('version')} -> {m.get('version')} (manual)")
        entries[id] = dict(m)

    catalog = {
        "schema_version": SCHEMA_VERSION,
        "kind": KIND,
        "serial": report["serial"],
        "generated_at": now_s,
        "expires": ts(now + datetime.timedelta(days=EXPIRES_DAYS)),
        "entries": [canonical(entries[k]) for k in sorted(entries)],
    }
    return catalog, report


ENTRY_KEYS = ("id", "vendor", "component", "match", "version", "version_scheme", "release_date",
              "download_url", "release_notes_url", "source", "checked_at")


def canonical(e):
    """Entry with its keys in the documented order (stable, reviewable diffs on the catalog branch)."""
    out = {k: e[k] for k in ENTRY_KEYS if k in e}
    out.update({k: v for k, v in e.items() if k not in out})
    return out


def check_manual(manual, schema, hosts, now):
    """PR check of catalog/manual.json -> (errors, warnings)."""
    errs, warns = [], []
    if manual.get("schema_version") != SCHEMA_VERSION:
        errs.append(f"schema_version must be {SCHEMA_VERSION}")
    entries = manual.get("entries")
    if not isinstance(entries, list):
        return errs + ["entries must be a list"], warns
    allow = manual.get("allow_downgrade", [])
    if not isinstance(allow, list) or not all(isinstance(x, str) for x in allow):
        errs.append("allow_downgrade must be a list of ids")
    wrapper = {
        "schema_version": SCHEMA_VERSION, "kind": KIND, "serial": 1,
        "generated_at": ts(now), "expires": ts(now + datetime.timedelta(days=EXPIRES_DAYS)), "entries": entries,
    }
    errs += validate(wrapper, schema, hosts, now + FUTURE_SLACK)
    for e in entries:
        if isinstance(e, dict) and e.get("source") != "manual":
            errs.append(f"{e.get('id')}: source must be 'manual' in manual.json")
    _, report = build(None, manual, {}, [], now, hosts)
    warns += [w for w in report["warnings"]]
    return errs, warns


def summary_markdown(catalog, report):
    lines = [f"### Driver catalog serial {catalog['serial']}", "",
             f"Generated {catalog['generated_at']}, expires {catalog['expires']}, {len(catalog['entries'])} entries.", ""]
    lines.append("| id | version | release date | source | checked |")
    lines.append("|---|---|---|---|---|")
    for e in catalog["entries"]:
        lines.append(f"| {e['id']} | {e['version']} | {e.get('release_date') or '-'} | {e['source']} | {e['checked_at']} |")
    for title, items in (("Changes", report["changes"]), ("Warnings", report["warnings"])):
        if items:
            lines += ["", f"**{title}**", ""] + [f"- {x}" for x in items]
    if report["failures"]:
        lines += ["", "**Adapter failures** (previous entries kept)", ""]
        lines += [f"- {f['adapter']} / {f['id']}: {f['error']}" for f in report["failures"]]
    return "\n".join(lines) + "\n"
