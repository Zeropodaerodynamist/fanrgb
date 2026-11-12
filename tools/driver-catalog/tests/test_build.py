"""Catalog building and validation, the schema validator, version vectors shared with the app, the
polite HTTP client and the issue planner."""

import copy
import datetime
import io
import json
import os
import unittest
import urllib.error

from drivercat import adapters, build, issues, net, versions
from drivercat import schema as jsonschema

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
FIX = os.path.join(os.path.dirname(__file__), "fixtures")
NOW = datetime.datetime(2026, 10, 19, 18, 0, 0, tzinfo=datetime.timezone.utc)


def load(*p):
    with open(os.path.join(*p), encoding="utf-8") as f:
        return json.load(f)


SCHEMA = load(ROOT, "catalog", "schema.json")
HOSTS = set(load(ROOT, "catalog", "allowed-hosts.json")["hosts"])
MANUAL = load(ROOT, "catalog", "manual.json")


def outcomes_from_fixtures():
    return build.run_adapters(adapters.ADAPTERS, fixtures=FIX)


class VersionVectorsTest(unittest.TestCase):
    """The same vectors as src/drivers/catalog/version.rs::shared_vectors_with_the_ci_fetcher."""

    def test_vectors(self):
        v = load(os.path.dirname(__file__), "version_vectors.json")
        want = {"newer": 1, "equal": 0, "older": -1, "none": None}
        for c in v["compare"]:
            self.assertEqual(versions.compare(c["scheme"], c["installed"], c["latest"]), want[c["expect"]], c)
        for c in v["invalid_latest"]:
            for bad in c["values"]:
                self.assertFalse(versions.is_valid(c["scheme"], bad), (c["scheme"], bad))
        self.assertTrue(versions.is_valid("gigabyte-bios", "F36d"))
        self.assertIsNone(versions.compare("semver", "1.0.0", "1.0.1"))


class BuildTest(unittest.TestCase):
    def first(self):
        cat, rep = build.build(None, MANUAL, outcomes_from_fixtures(), adapters.ADAPTERS, NOW, HOSTS)
        return cat, rep

    def test_first_catalog_from_fixtures(self):
        cat, rep = self.first()
        self.assertEqual(cat["serial"], 1)
        self.assertEqual(cat["generated_at"], "2026-10-19T18:00:00Z")
        self.assertEqual(cat["expires"], "2026-11-09T18:00:00Z", "21 days")
        got = {e["id"]: (e["version"], e["source"]) for e in cat["entries"]}
        self.assertEqual(got, {
            "amd-chipset-am5": ("8.08.12.551", "manual"),  # the manual pin replaces the (synthetic) auto result
            "gigabyte-b650m-d3hp-ax-r13-bios": ("F36d", "auto:gigabyte-bios"),
            "intel-arc-graphics": ("32.0.101.9150", "auto:intel-arc"),
            "realtek-lan-ndis": ("10.80.50", "auto:realtek-lan"),
            "realtek-lan-netadaptercx": ("11.031.50", "auto:realtek-lan"),
        })
        self.assertEqual(list(cat["entries"][0].keys())[:5], ["id", "vendor", "component", "match", "version"])
        self.assertEqual(rep["failures"], [])
        self.assertEqual(build.validate(cat, SCHEMA, HOSTS, NOW), [])

    def test_the_live_first_catalog_is_valid(self):
        live = load(FIX, "first-catalog.json")
        self.assertEqual(build.validate(live, SCHEMA, HOSTS, build.parse_ts(live["generated_at"])), [])

    def test_failure_keeps_the_previous_entry(self):
        prev, _ = self.first()
        out = outcomes_from_fixtures()
        out["intel-arc"] = {"ok": False, "error": "www.intel.com: HTTP 503 (after 3 attempts)"}
        later = NOW + datetime.timedelta(days=7)
        cat, rep = build.build(prev, MANUAL, out, adapters.ADAPTERS, later, HOSTS)
        intel = next(e for e in cat["entries"] if e["id"] == "intel-arc-graphics")
        self.assertEqual(intel["checked_at"], "2026-10-19T18:00:00Z", "checked_at is not refreshed")
        self.assertEqual(intel["version"], "32.0.101.9150")
        self.assertEqual(cat["serial"], 2)
        other = next(e for e in cat["entries"] if e["id"] == "realtek-lan-ndis")
        self.assertEqual(other["checked_at"], "2026-10-26T18:00:00Z", "the other vendors are refreshed")
        self.assertEqual([(f["adapter"], f["kept_previous"]) for f in rep["failures"]], [("intel-arc", True)])
        self.assertEqual(build.validate(cat, SCHEMA, HOSTS, later, prev), [])
        # A failing adapter with nothing published yet simply has no entry.
        cat, rep = build.build(None, MANUAL, out, adapters.ADAPTERS, NOW, HOSTS)
        self.assertNotIn("intel-arc-graphics", [e["id"] for e in cat["entries"]])
        self.assertFalse(rep["failures"][0]["kept_previous"])

    def test_parser_exception_never_blocks_others(self):
        class Broken(adapters.IntelArc):
            def parse(self, html):
                raise KeyError("boom")
        out = build.run_adapters([Broken(), adapters.RealtekLan()], fixtures=FIX)
        self.assertFalse(out["intel-arc"]["ok"])
        self.assertIn("KeyError", out["intel-arc"]["error"])
        self.assertTrue(out["realtek-lan"]["ok"])

    def test_downgrade_is_refused_unless_allowed(self):
        prev, _ = self.first()
        out = outcomes_from_fixtures()
        out["intel-arc"]["results"]["intel-arc-graphics"]["version"] = "32.0.101.9034"
        cat, rep = build.build(prev, MANUAL, out, adapters.ADAPTERS, NOW, HOSTS)
        intel = next(e for e in cat["entries"] if e["id"] == "intel-arc-graphics")
        self.assertEqual(intel["version"], "32.0.101.9150")
        self.assertIn("downgrade refused", rep["failures"][0]["error"])
        cat, rep = build.build(prev, MANUAL, out, adapters.ADAPTERS, NOW, HOSTS, allow_downgrade=["intel-arc-graphics"])
        self.assertEqual(next(e for e in cat["entries"] if e["id"] == "intel-arc-graphics")["version"], "32.0.101.9034")
        self.assertEqual(rep["failures"], [])
        # validate() (the signing job) enforces the same rule independently.
        errs = build.validate(cat, SCHEMA, HOSTS, NOW, prev)
        self.assertTrue(any("downgrade" in e for e in errs), errs)
        self.assertEqual(build.validate(cat, SCHEMA, HOSTS, NOW, prev, {"intel-arc-graphics"}), [])

    def test_bad_adapter_output_is_not_published(self):
        out = outcomes_from_fixtures()
        out["realtek-lan"]["results"]["realtek-lan-ndis"]["version"] = "10.80"
        out["intel-arc"]["results"]["intel-arc-graphics"]["release_notes_url"] = "https://evil.example/notes.pdf"
        cat, rep = build.build(None, MANUAL, out, adapters.ADAPTERS, NOW, HOSTS)
        ids = [e["id"] for e in cat["entries"]]
        self.assertNotIn("realtek-lan-ndis", ids)
        self.assertNotIn("intel-arc-graphics", ids)
        self.assertEqual(len(rep["failures"]), 2)
        self.assertEqual(build.validate(cat, SCHEMA, HOSTS, NOW), [])

    def test_manual_entries(self):
        manual = copy.deepcopy(MANUAL)
        manual["entries"][0]["checked_at"] = "2026-07-01T00:00:00Z"
        manual["entries"][0]["version"] = "8.03.25.247"
        out = outcomes_from_fixtures()  # the auto adapter (synthetic page) sees 8.08.12.551
        cat, rep = build.build(None, manual, out, adapters.ADAPTERS, NOW, HOSTS)
        amd = next(e for e in cat["entries"] if e["id"] == "amd-chipset-am5")
        self.assertEqual((amd["version"], amd["source"]), ("8.03.25.247", "manual"), "manual replaces auto")
        self.assertTrue(any("newer than the manual pin" in w for w in rep["warnings"]), rep["warnings"])
        self.assertTrue(any("110 days ago" in w for w in rep["warnings"]), rep["warnings"])
        errs, warns = build.check_manual(manual, SCHEMA, HOSTS, NOW)
        self.assertEqual(errs, [])
        self.assertTrue(any("days ago" in w for w in warns))
        # The repository's manual.json passes as it is.
        self.assertEqual(build.check_manual(MANUAL, SCHEMA, HOSTS, NOW)[0], [])

    def test_manual_validation_errors(self):
        for change, why in [
            (lambda m: m["entries"][0].update(source="auto:amd-chipset"), "source"),
            (lambda m: m["entries"][0].update(download_url="https://amd.example/x"), "host"),
            (lambda m: m["entries"][0].update(download_url="http://www.amd.com/x"), "https"),
            (lambda m: m["entries"][0].update(vendor="AMD‮"), "bidi"),
            (lambda m: m["entries"][0].update(vendor="A" * 121), "length"),
            (lambda m: m["entries"][0].update(version="8.8.12.551"), "version shape"),
            (lambda m: m["entries"][0].update(checked_at="2026-11-30T00:00:00Z"), "future"),
            (lambda m: m["entries"][0].update(release_date="2026-02-30"), "date"),
            (lambda m: m["entries"][0]["match"].update(packages=[]), "no key rule"),
            (lambda m: m["entries"][0]["match"].update(hardware_ids=["PCI\\*"]), "short pattern"),
            (lambda m: m["entries"].append(copy.deepcopy(m["entries"][0])), "duplicate"),
            (lambda m: m.update(schema_version="2.0"), "schema"),
        ]:
            m = copy.deepcopy(MANUAL)
            change(m)
            self.assertNotEqual(build.check_manual(m, SCHEMA, HOSTS, NOW)[0], [], why)

    def test_serial_and_dates(self):
        prev, _ = self.first()
        cat, _ = build.build(prev, MANUAL, outcomes_from_fixtures(), adapters.ADAPTERS, NOW, HOSTS)
        self.assertEqual(cat["serial"], 2)
        bad = dict(cat, serial=5)
        self.assertTrue(any("serial" in e for e in build.validate(bad, SCHEMA, HOSTS, NOW, prev)))
        self.assertTrue(any("serial must be 1" in e for e in build.validate(dict(cat, serial=3), SCHEMA, HOSTS, NOW)))
        late = dict(cat, expires="2027-06-01T00:00:00Z")
        self.assertTrue(any("90 days" in e for e in build.validate(late, SCHEMA, HOSTS, NOW, prev)))
        future = dict(cat, generated_at="2026-10-25T00:00:00Z", expires="2026-11-01T00:00:00Z")
        self.assertTrue(any("future" in e for e in build.validate(future, SCHEMA, HOSTS, NOW, prev)))

    def test_summary(self):
        cat, rep = self.first()
        md = build.summary_markdown(cat, rep)
        self.assertIn("| intel-arc-graphics | 32.0.101.9150 | 2026-10-02 | auto:intel-arc |", md)


class TextAndUrlTest(unittest.TestCase):
    def test_hidden_characters_match_the_app(self):
        for c in "‮⁦​﻿­\x07\U000e0041￾":
            self.assertTrue(build.is_hidden(c), hex(ord(c)))
        for c in "aÄ中®™–":
            self.assertFalse(build.is_hidden(c), c)
        self.assertFalse(build.text_ok("Intel\tArc"))
        self.assertFalse(build.text_ok("  "))
        self.assertTrue(build.text_ok("Intel® Arc™ Graphics – Windows*"))

    def test_url_allow_list_matches_the_app(self):
        ok = ["https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html",
              "https://downloadmirror.intel.com/930514/ReleaseNotes__101.9150_WHQL.pdf",
              "https://www.realtek.com/Download/List?cate_id=584",
              "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-13/support#support-dl-bios",
              "https://pawnio.eu/"]
        for u in ok:
            self.assertTrue(build.url_ok(u, HOSTS), u)
        for u in ["http://www.intel.com/x", "https://intel.com/x", "https://www.intel.com.evil.example/x",
                  "https://www.intel.com@evil.example/x", "https://WWW.INTEL.COM/x", "https://www.intel.com:443/x",
                  "https://www.intel.com/a b", "https://www.intel.com/%2e", "https://www.intel.com/x'y",
                  "https://www.intel.com?x", "javascript:alert(1)", "https://www.intel.com/" + "a" * 300]:
            self.assertFalse(build.url_ok(u, HOSTS), u)


class SchemaValidatorTest(unittest.TestCase):
    def test_keywords(self):
        s = {"type": "object", "required": ["a"], "properties": {
            "a": {"type": "string", "pattern": "^x+$", "maxLength": 3},
            "b": {"type": "array", "maxItems": 1, "items": {"enum": [1, 2]}},
            "c": {"oneOf": [{"type": "null"}, {"type": "integer", "minimum": 1}]}}}
        self.assertEqual(jsonschema.errors({"a": "xx", "b": [1], "c": None}, s), [])
        self.assertTrue(jsonschema.errors({"a": "xx\n"}, s), "a final $ does not match before a newline")
        self.assertTrue(jsonschema.errors({"a": "xxxx"}, s))
        self.assertTrue(jsonschema.errors({"b": [3, 1]}, s))
        self.assertTrue(jsonschema.errors({"a": "x", "c": 0}, s))
        self.assertTrue(jsonschema.errors([], s))
        self.assertTrue(jsonschema.errors({"a": "x", "b": True}, s))

    def test_repo_schema_against_the_live_catalog(self):
        live = load(FIX, "first-catalog.json")
        self.assertEqual(jsonschema.errors(live, SCHEMA), [])
        bad = copy.deepcopy(live)
        bad["entries"][0]["version_scheme"] = "semver"
        bad["kind"] = "other"
        bad["entries"][1]["download_url"] = "ftp://x"
        self.assertEqual(len(jsonschema.errors(bad, SCHEMA)), 3)


class FakeResponse(io.BytesIO):
    def __init__(self, body, headers=None):
        super().__init__(body)
        self.headers = _Headers(headers or {})

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class _Headers(dict):
    def get_content_charset(self):
        return "utf-8"

    def get(self, k, d=None):
        return dict.get(self, k, d)


class FakeOpener:
    def __init__(self, answers):
        self.answers = list(answers)
        self.seen = []

    def open(self, req, timeout):
        self.seen.append((req.full_url, dict(req.header_items()), timeout))
        a = self.answers.pop(0)
        if isinstance(a, Exception):
            raise a
        return a


def http_error(code, headers=None):
    return urllib.error.HTTPError("https://www.intel.com/x", code, "x", headers or {}, None)


class HttpTest(unittest.TestCase):
    def make(self, answers, retries=2):
        self.sleeps = []
        self.t = [0.0]
        o = FakeOpener(answers)
        h = net.Http({"www.intel.com", "www.realtek.com"}, sleep=self.sleeps.append, clock=lambda: self.t[0], opener=o, retries=retries)
        return h, o

    def test_polite_headers_and_spacing(self):
        h, o = self.make([FakeResponse(b"one"), FakeResponse(b"two")])
        self.assertEqual(h.get("https://www.intel.com/a"), "one")
        self.assertEqual(h.get("https://www.intel.com/b"), "two")
        hdrs = o.seen[0][1]
        self.assertIn("FanRGB-driver-catalog", hdrs["User-agent"])
        self.assertNotIn("Mozilla", hdrs["User-agent"], "no browser impersonation")
        self.assertEqual(o.seen[0][2], net.TIMEOUT)
        self.assertEqual(self.sleeps, [net.MIN_INTERVAL], "same host: waits between requests")

    def test_retries_only_transient_errors(self):
        h, o = self.make([http_error(503, {"Retry-After": "7"}), TimeoutError("slow"), FakeResponse(b"ok")])
        self.assertEqual(h.get("https://www.intel.com/a"), "ok")
        self.assertEqual(len(o.seen), 3)
        self.assertIn(7, self.sleeps, "Retry-After honoured")
        h, o = self.make([http_error(403)])
        with self.assertRaises(net.FetchError) as e:
            h.get("https://www.intel.com/a")
        self.assertEqual(len(o.seen), 1, "403 is not retried")
        self.assertIn("HTTP 403", str(e.exception))
        h, o = self.make([http_error(429, {"Retry-After": "9999"}), http_error(429), http_error(429)])
        with self.assertRaises(net.FetchError):
            h.get("https://www.intel.com/a")
        self.assertLessEqual(max(self.sleeps), net.MAX_RETRY_AFTER)
        self.assertEqual(len(o.seen), 3)

    def test_allow_list_and_size_cap(self):
        h, o = self.make([])
        for u in ["http://www.intel.com/a", "https://evil.example/a", "file:///etc/passwd"]:
            with self.assertRaises(net.FetchError):
                h.get(u)
        self.assertEqual(o.seen, [])
        h, o = self.make([FakeResponse(b"x" * (net.MAX_BODY + 1))])
        with self.assertRaises(net.FetchError):
            h.get("https://www.intel.com/a")

    def test_redirects_stay_on_allowed_hosts(self):
        r = net._Redirects({"www.intel.com"})
        with self.assertRaises(net.FetchError):
            r.redirect_request(None, None, 302, "x", {}, "https://evil.example/x")
        with self.assertRaises(net.FetchError):
            r.redirect_request(None, None, 302, "x", {}, "http://www.intel.com/x")
        self.assertEqual(r.max_redirections, 3)


class IssuePlanTest(unittest.TestCase):
    NAMES = ["intel-arc", "amd-chipset"]

    def test_open_update_close(self):
        rep = {"failures": [{"adapter": "amd-chipset", "id": "amd-chipset-am5", "error": "www.amd.com: HTTP 403", "kept_previous": True}],
               "warnings": ["amd-chipset-am5: manual entry last checked 70 days ago"]}
        acts = issues.plan(rep, set(), True, "https://run", self.NAMES)
        self.assertEqual([(a, t) for a, t, _ in acts], [
            ("create", "Driver catalog: amd-chipset adapter failing"),
            ("create", "Driver catalog: entries need attention"),
        ])
        self.assertIn("previously published entry was kept", acts[0][2])
        opened = {"Driver catalog: amd-chipset adapter failing", "Driver catalog: intel-arc adapter failing",
                  "Driver catalog: entries need attention"}
        acts = issues.plan(rep, opened, False, "https://run", self.NAMES)
        self.assertEqual([(a, t) for a, t, _ in acts], [
            ("close", "Driver catalog: intel-arc adapter failing"),
            ("comment", "Driver catalog: amd-chipset adapter failing"),
            ("comment", "Driver catalog: entries need attention"),
            ("create", "Driver catalog: publishing failed"),
        ])

    def test_sync_uses_gh_without_a_shell(self):
        calls = []

        class R:
            def __init__(self, out):
                self.returncode, self.stdout, self.stderr = 0, out, ""

        def runner(args, input=None, **kw):
            calls.append((args, input))
            return R("[]" if args[1:3] == ["issue", "list"] else "")
        rep = {"failures": [{"adapter": "intel-arc", "id": "x", "error": "e`$(rm -rf /)`", "kept_previous": False}], "warnings": []}
        issues.sync(rep, True, "https://run", self.NAMES, "o/r", runner=runner)
        create = [c for c in calls if c[0][1:3] == ["issue", "create"]][0]
        self.assertEqual(create[0][0], "gh")
        self.assertIn("--body-file", create[0])
        self.assertIn("$(rm -rf /)", create[1], "untrusted text goes through stdin as data")


if __name__ == "__main__":
    unittest.main()
