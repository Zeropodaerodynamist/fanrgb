#!/usr/bin/env python3
"""FanRGB driver catalog builder (run by .github/workflows/driver-catalog.yml; standard library only).

  catalog.py build --repo-root . --previous previous/catalog/drivers.json --out out
                   [--allow-downgrade id,id] [--fixtures DIR] [--save-pages DIR]
      Fetches every adapter's vendor page (or reads saved pages with --fixtures), builds the next
      catalog and writes out/drivers.json (unsigned), out/report.json and out/summary.md. Adapter
      failures are reported, not fatal; an invalid catalog exits 2.
  catalog.py validate --repo-root . --catalog out/drivers.json [--previous previous/catalog/drivers.json]
                   [--allow-downgrade id,id]
      The checks of the signing job (schema, app rules, serial = published + 1, no downgrade).
  catalog.py check-manual --repo-root .
      Pull-request check of catalog/manual.json (errors fail; stale entries are warnings).
  catalog.py issues --report out/report.json --publish-ok true|false --run-url URL --repo owner/name
      Opens/updates/closes the tracking issues (needs GH_TOKEN with issues: write).
"""

import argparse
import datetime
import json
import os
import sys

# Our own package next to this file (works with `python3 -I`, which leaves the script folder off sys.path).
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from drivercat import adapters as adapters_mod  # noqa: E402
from drivercat import build as builder  # noqa: E402
from drivercat import issues  # noqa: E402
from drivercat.net import Http  # noqa: E402

MAX_JSON = 1024 * 1024


def load_json(path, required=True):
    if not path or not os.path.exists(path):
        if required:
            raise SystemExit(f"error: {path} not found")
        return None
    if os.path.getsize(path) > MAX_JSON:
        raise SystemExit(f"error: {path} is larger than {MAX_JSON} bytes")
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def write(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)


def dump(obj):
    # Stable bytes: these are what gets signed.
    return json.dumps(obj, indent=2, ensure_ascii=False) + "\n"


def repo_files(root):
    schema = load_json(os.path.join(root, "catalog", "schema.json"))
    hosts = set(load_json(os.path.join(root, "catalog", "allowed-hosts.json"))["hosts"])
    manual = load_json(os.path.join(root, "catalog", "manual.json"))
    return schema, hosts, manual


def ids(s):
    return [x.strip() for x in (s or "").split(",") if x.strip()]


def utcnow():
    return datetime.datetime.now(datetime.timezone.utc)


def gh_annotation(kind, msg):
    if os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::{kind}::{msg.replace(chr(10), ' ')}")
    else:
        print(f"{kind}: {msg}")


def cmd_build(a):
    schema, hosts, manual = repo_files(a.repo_root)
    previous = load_json(a.previous, required=False)
    if previous is not None:
        errs = builder.jsonschema.errors(previous, schema)
        if errs:
            raise SystemExit(f"error: the published catalog does not match the schema: {errs[:3]}")
    if a.save_pages:
        os.makedirs(a.save_pages, exist_ok=True)
    http = None if a.fixtures else Http(hosts, retries=a.retries)
    outcomes = builder.run_adapters(adapters_mod.ADAPTERS, http=http, fixtures=a.fixtures, save=a.save_pages)
    now = utcnow()
    catalog, report = builder.build(previous, manual, outcomes, adapters_mod.ADAPTERS, now, hosts, ids(a.allow_downgrade))
    allow = set(ids(a.allow_downgrade)) | set(manual.get("allow_downgrade", []))
    errs = builder.validate(catalog, schema, hosts, now, previous, allow)
    report["requests"] = http.requests if http else 0
    report["adapters"] = {n: o["ok"] for n, o in outcomes.items()}
    write(os.path.join(a.out, "report.json"), dump(report))
    write(os.path.join(a.out, "summary.md"), builder.summary_markdown(catalog, report))
    for f in report["failures"]:
        gh_annotation("warning", f"{f['adapter']} / {f['id']}: {f['error']}")
    for w in report["warnings"]:
        gh_annotation("warning", w)
    if errs:
        for e in errs:
            gh_annotation("error", e)
        return 2
    write(os.path.join(a.out, "drivers.json"), dump(catalog))
    print(f"built catalog serial {catalog['serial']} with {len(catalog['entries'])} entries "
          f"({sum(report['adapters'].values())}/{len(report['adapters'])} adapters worked, {report['requests']} requests)")
    return 0


def cmd_validate(a):
    schema, hosts, manual = repo_files(a.repo_root)
    with open(a.catalog, "rb") as f:
        raw = f.read(MAX_JSON + 1)
    if len(raw) > 256 * 1024:
        gh_annotation("error", "the catalog is larger than the app's 256 KiB cap")
        return 2
    catalog = json.loads(raw.decode("utf-8"))
    previous = load_json(a.previous, required=False)
    allow = set(ids(a.allow_downgrade)) | set(manual.get("allow_downgrade", []))
    errs = builder.validate(catalog, schema, hosts, utcnow(), previous, allow)
    for e in errs:
        gh_annotation("error", e)
    if not errs:
        print(f"catalog serial {catalog['serial']} is valid ({len(catalog['entries'])} entries)")
    return 2 if errs else 0


def cmd_check_manual(a):
    schema, hosts, manual = repo_files(a.repo_root)
    errs, warns = builder.check_manual(manual, schema, hosts, utcnow())
    for w in warns:
        gh_annotation("warning", w)
    for e in errs:
        gh_annotation("error", e)
    if not errs:
        print(f"catalog/manual.json: {len(manual.get('entries', []))} entries valid, {len(warns)} warning(s)")
    return 1 if errs else 0


def cmd_issues(a):
    report = load_json(a.report, required=False) or {"failures": [], "warnings": []}
    done = issues.sync(report, a.publish_ok == "true", a.run_url, [x.name for x in adapters_mod.ADAPTERS], a.repo)
    for action, title in done:
        print(f"{action}: {title}")
    return 0


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--repo-root", default=".")
    b.add_argument("--previous")
    b.add_argument("--out", required=True)
    b.add_argument("--allow-downgrade", default="")
    b.add_argument("--fixtures")
    b.add_argument("--save-pages")
    b.add_argument("--retries", type=int, default=2, choices=range(0, 4))
    v = sub.add_parser("validate")
    v.add_argument("--repo-root", default=".")
    v.add_argument("--catalog", required=True)
    v.add_argument("--previous")
    v.add_argument("--allow-downgrade", default="")
    m = sub.add_parser("check-manual")
    m.add_argument("--repo-root", default=".")
    i = sub.add_parser("issues")
    i.add_argument("--report", required=True)
    i.add_argument("--publish-ok", choices=["true", "false"], required=True)
    i.add_argument("--run-url", required=True)
    i.add_argument("--repo", required=True)
    a = p.parse_args(argv)
    return {"build": cmd_build, "validate": cmd_validate, "check-manual": cmd_check_manual, "issues": cmd_issues}[a.cmd](a)


if __name__ == "__main__":
    sys.exit(main())
