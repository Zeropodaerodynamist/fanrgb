"""Adapter parsers against saved vendor pages (tests/fixtures).

Excerpts of real captures (2026-10-19, by the fetcher itself with its honest User-Agent), keeping only the
elements the parsers read, byte for byte (see the comment at the top of each file):
  intel-arc.html    https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html
  realtek-lan.html  https://www.realtek.com/Download/List?cate_id=584
Synthetic (the vendor blocks non-browser clients; see the comment at the top of each file):
  amd-b650.html, gigabyte-b650m-d3hp-ax-r13.html
"""

import os
import unittest

from drivercat import adapters
from drivercat.adapters import AdapterError

FIX = os.path.join(os.path.dirname(__file__), "fixtures")


def page(name):
    with open(os.path.join(FIX, name), encoding="utf-8") as f:
        return f.read()


class IntelArcTest(unittest.TestCase):
    def test_real_page(self):
        r = adapters.IntelArc().parse(page("intel-arc.html"))
        self.assertEqual(r, {"intel-arc-graphics": {
            "version": "32.0.101.9150",
            "release_date": "2026-10-02",
            "release_notes_url": "https://downloadmirror.intel.com/930514/ReleaseNotes__101.9150_WHQL.pdf",
        }})

    def test_changed_page_is_refused(self):
        html = page("intel-arc.html")
        a = adapters.IntelArc()
        with self.assertRaises(AdapterError):
            a.parse(html.replace('name="DownloadVersion"', 'name="SomethingElse"'))
        with self.assertRaises(AdapterError):  # selector and meta disagree
            a.parse(html.replace("32.0.101.9150 WHQL Certified (Latest)", "32.0.101.9200 (Latest)"))
        with self.assertRaises(AdapterError):  # not an Intel graphics version
            a.parse(html.replace('content="32.0.101.9150 WHQL Certified"', 'content="9150 WHQL"'))
        with self.assertRaises(AdapterError):
            a.parse("<html><body>Access Denied</body></html>")
        # Release notes of another build are not attached.
        r = a.parse(html.replace("ReleaseNotes__101.9150_WHQL.pdf", "ReleaseNotes__101.9034.pdf"))
        self.assertIsNone(r["intel-arc-graphics"]["release_notes_url"])
        # A date in the far future is a parser problem, not a release.
        with self.assertRaises(AdapterError):
            a.parse(html.replace('name="LastUpdate" content="10/02/2026', 'name="LastUpdate" content="10/02/2099'))


class RealtekLanTest(unittest.TestCase):
    def test_real_page(self):
        r = adapters.RealtekLan().parse(page("realtek-lan.html"))
        self.assertEqual(r["realtek-lan-ndis"], {"version": "10.80.50", "release_date": "2026-10-05", "release_notes_url": None})
        self.assertEqual(r["realtek-lan-netadaptercx"],
                         {"version": "11.031.50", "release_date": "2026-10-05", "release_notes_url": None})

    def test_changed_page_is_refused(self):
        html = page("realtek-lan.html")
        a = adapters.RealtekLan()
        with self.assertRaises(AdapterError):  # the row was renamed
            a.parse(html.replace("Win11 Auto Installation Program (NetAdapterCx)</td>", "Win11 Installer</td>"))
        with self.assertRaises(AdapterError):  # family mix-up: an 11.x version in the NDIS row
            a.parse(html.replace("<td>10.80.50</td>", "<td>11.80.50</td>"))
        with self.assertRaises(AdapterError):
            a.parse(html.replace("<td>10.80.50</td>", "<td>latest</td>"))
        with self.assertRaises(AdapterError):
            a.parse(html.replace('<h2 class="title">Windows</h2>', '<h2 class="title">Win</h2>'))


class AmdChipsetTest(unittest.TestCase):
    def test_synthetic_page(self):
        r = adapters.AmdChipset().parse(page("amd-b650.html"))
        self.assertEqual(r["amd-chipset-am5"]["version"], "8.08.12.551")
        self.assertEqual(r["amd-chipset-am5"]["release_date"], "2026-08-14")
        self.assertEqual(r["amd-chipset-am5"]["release_notes_url"],
                         "https://www.amd.com/en/resources/support-articles/release-notes/RN-RYZEN-CHIPSET-8-08-12-551.html")

    def test_blocked_or_changed(self):
        a = adapters.AmdChipset()
        with self.assertRaises(AdapterError):
            a.parse("<html><body><h1>Access Denied</h1></body></html>")
        with self.assertRaises(AdapterError):  # the script's text is never read as page content
            a.parse(page("amd-b650.html").replace("<h3>AMD Chipset Drivers</h3>", "<h3>Chipset</h3>"))


class GigabyteBiosTest(unittest.TestCase):
    def test_synthetic_page(self):
        r = adapters.GigabyteBios().parse(page("gigabyte-b650m-d3hp-ax-r13.html"))
        self.assertEqual(r, {"gigabyte-b650m-d3hp-ax-r13-bios": {"version": "F36d", "release_date": "2025-09-18", "release_notes_url": None}})

    def test_ordering_and_decoys(self):
        html = page("gigabyte-b650m-d3hp-ax-r13.html").replace(">F33<", ">F36e<")
        self.assertEqual(adapters.GigabyteBios().parse(html)["gigabyte-b650m-d3hp-ax-r13-bios"]["version"], "F36e")
        with self.assertRaises(AdapterError):  # a JavaScript-only page has no rows
            adapters.GigabyteBios().parse("<html><body><h3>BIOS</h3><div id='app'></div></body></html>")
        with self.assertRaises(AdapterError):
            adapters.GigabyteBios().parse("<html><body>403 Forbidden</body></html>")


class StaticEntriesTest(unittest.TestCase):
    def test_ids_are_unique_and_adapters_named(self):
        ids = [i for a in adapters.ADAPTERS for i in a.ids()]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual([a.name for a in adapters.ADAPTERS], ["intel-arc", "amd-chipset", "realtek-lan", "gigabyte-bios"])
        for a in adapters.ADAPTERS:
            self.assertTrue(os.path.exists(os.path.join(FIX, a.fixture)), a.fixture)


if __name__ == "__main__":
    unittest.main()
