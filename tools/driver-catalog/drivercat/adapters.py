"""One adapter per vendor source. Each knows the page it reads, the catalog entries it produces (their
fixed parts: id, vendor, component, match rules, version scheme, download page) and how to read the
version and date from the page. Parsers are defensive: anything unexpected raises AdapterError, and the
build keeps the previous entry for that adapter instead of guessing.

The match rules and pages are reviewed code (this file), never taken from a vendor page.
"""

import datetime
import re

from . import versions
from .page import Page, norm

INTEL_ARC_PAGE = "https://www.intel.com/content/www/us/en/download/785597/intel-arc-graphics-windows.html"
AMD_B650_PAGE = "https://www.amd.com/en/support/downloads/drivers.html/chipsets/am5/b650.html"
REALTEK_LAN_PAGE = "https://www.realtek.com/Download/List?cate_id=584"
GIGABYTE_D3HP_R13 = "https://www.gigabyte.com/Motherboard/B650M-D3HP-AX-rev-13/support"


class AdapterError(Exception):
    """The page did not look as expected (message goes into the GitHub issue; never page content)."""


def _date(y, m, d):
    try:
        dt = datetime.date(int(y), int(m), int(d))
    except ValueError:
        raise AdapterError(f"invalid date {y}-{m}-{d}") from None
    if dt.year < 2015 or dt > datetime.date.today() + datetime.timedelta(days=2):
        raise AdapterError(f"implausible release date {dt.isoformat()}")
    return dt.isoformat()


def _entry(id, vendor, component, match, scheme, download_url):
    return {
        "id": id, "vendor": vendor, "component": component, "match": match,
        "version_scheme": scheme, "download_url": download_url,
    }


class Adapter:
    name = ""
    url = ""
    fixture = ""

    def entries(self):
        raise NotImplementedError

    def parse(self, html):
        """-> {entry id: {"version", "release_date", optional "release_notes_url"}}"""
        raise NotImplementedError

    def ids(self):
        return [e["id"] for e in self.entries()]


# ---------------------------------------------------------------------------------------
# Intel Arc (intel.com download centre)

class IntelArc(Adapter):
    name = "intel-arc"
    url = INTEL_ARC_PAGE
    fixture = "intel-arc.html"
    _NOTES = re.compile(r"^https://downloadmirror\.intel\.com/[0-9]{4,8}/[A-Za-z0-9._-]{1,80}\.pdf$")

    def entries(self):
        return [_entry(
            "intel-arc-graphics", "Intel", "Intel Arc & Iris Xe Graphics driver",
            {
                "categories": ["gpu"],
                # Alchemist (DG2 56xx), Battlemage (E2xx), Meteor/Arrow Lake (7Dxx), Lunar Lake (64xx).
                "hardware_ids": ["PCI\\VEN_8086&DEV_56*", "PCI\\VEN_8086&DEV_E2*", "PCI\\VEN_8086&DEV_7D*", "PCI\\VEN_8086&DEV_64*"],
                "providers": ["Intel"],
            },
            "intel-gfx", INTEL_ARC_PAGE,
        )]

    def parse(self, html):
        p = Page(html)
        meta_v = norm(p.meta.get("DownloadVersion", ""))
        m = re.match(r"^([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)(\s|$)", meta_v)
        if not m:
            raise AdapterError("no DownloadVersion meta tag with a driver version")
        version = m.group(1)
        if not versions.is_valid("intel-gfx", version):
            raise AdapterError(f"version {version!r} is not an Intel graphics version")
        # The version selector must agree: its "(Latest)" option names the same build.
        latest = [t for _, t in p.options if "(Latest)" in t]
        if latest and not latest[0].startswith(version + " ") and latest[0] != version:
            raise AdapterError("the (Latest) version in the selector differs from the page's DownloadVersion")
        d = re.match(r"^([0-9]{1,2})/([0-9]{1,2})/([0-9]{4})", norm(p.meta.get("LastUpdate") or p.meta.get("lastModifieddate") or ""))
        release_date = _date(d.group(3), d.group(1), d.group(2)) if d else None
        build = ".".join(version.split(".")[2:])
        notes = next((h for h, t in p.links if self._NOTES.match(h) and "releasenotes" in h.lower() and build in h), None)
        return {"intel-arc-graphics": {"version": version, "release_date": release_date, "release_notes_url": notes}}


# ---------------------------------------------------------------------------------------
# AMD chipset (amd.com; often blocks non-browser clients)

class AmdChipset(Adapter):
    name = "amd-chipset"
    url = AMD_B650_PAGE
    fixture = "amd-b650.html"
    _REV = re.compile(
        r"AMD Chipset Drivers\b.{0,600}?Revision Number\s*:?\s*([0-9]{1,2}\.[0-9]{2}\.[0-9]{2}\.[0-9]{3,4})\b", re.S | re.I)
    _DATE = re.compile(r"Release Date\s*:?\s*([0-9]{1,2})/([0-9]{1,2})/([0-9]{4})", re.I)

    def entries(self):
        return [_entry(
            "amd-chipset-am5", "AMD", "AMD Chipset Software (AM5)",
            {"categories": ["chipset"], "packages": ["AMD Chipset Software"]},
            "amd-chipset", AMD_B650_PAGE,
        )]

    def parse(self, html):
        p = Page(html)
        m = self._REV.search(p.text)
        if not m:
            raise AdapterError("no 'AMD Chipset Drivers' block with a Revision Number")
        version = m.group(1)
        # The release date of that block: the nearest "Release Date" after the heading.
        block = p.text[m.start(): m.end() + 400]
        d = self._DATE.search(block)
        release_date = _date(d.group(3), d.group(1), d.group(2)) if d else None
        dashed = version.replace(".", "-")
        notes = next((h for h, _ in p.links
                      if h == f"https://www.amd.com/en/resources/support-articles/release-notes/RN-RYZEN-CHIPSET-{dashed}.html"), None)
        return {"amd-chipset-am5": {"version": version, "release_date": release_date, "release_notes_url": notes}}


# ---------------------------------------------------------------------------------------
# Realtek PCIe GbE / 2.5GbE family (realtek.com)

class RealtekLan(Adapter):
    name = "realtek-lan"
    url = REALTEK_LAN_PAGE
    fixture = "realtek-lan.html"
    _ROWS = {
        "realtek-lan-ndis": ("win10/win11 auto installation program (ndis)", 10),
        "realtek-lan-netadaptercx": ("win11 auto installation program (netadaptercx)", 11),
    }
    _MATCH = {
        "categories": ["lan"],
        "hardware_ids": ["PCI\\VEN_10EC&DEV_8125*", "PCI\\VEN_10EC&DEV_8126*", "PCI\\VEN_10EC&DEV_8127*",
                         "PCI\\VEN_10EC&DEV_8168*", "PCI\\VEN_10EC&DEV_8161*"],
        "providers": ["Realtek"],
    }

    def entries(self):
        return [
            _entry("realtek-lan-ndis", "Realtek", "Realtek PCIe GbE/2.5GbE driver (NDIS, Windows 10/11)",
                   self._MATCH, "realtek-lan", REALTEK_LAN_PAGE),
            _entry("realtek-lan-netadaptercx", "Realtek", "Realtek PCIe GbE/2.5GbE driver (NetAdapterCx, Windows 11)",
                   self._MATCH, "realtek-lan", REALTEK_LAN_PAGE),
        ]

    def parse(self, html):
        p = Page(html)
        rows = [r for t in p.tables if t["heading"].lower() == "windows" for r in t["rows"]]
        if not rows:
            raise AdapterError("no 'Windows' download table")
        out = {}
        for id, (desc, family) in self._ROWS.items():
            hit = [r for r in rows if len(r) >= 4 and norm(r[1]).lower() == desc]
            if len(hit) != 1:
                raise AdapterError(f"expected one '{desc}' row, found {len(hit)}")
            version, date = hit[0][2], hit[0][3]
            if not versions.is_valid("realtek-lan", version) or not version.startswith(f"{family}."):
                raise AdapterError(f"{id}: version {version[:20]!r} is not a {family}.x Realtek package version")
            d = re.fullmatch(r"([0-9]{4})/([0-9]{2})/([0-9]{2})", date)
            if not d:
                raise AdapterError(f"{id}: date {date[:20]!r} is not YYYY/MM/DD")
            out[id] = {"version": version, "release_date": _date(*d.groups()), "release_notes_url": None}
        return out


# ---------------------------------------------------------------------------------------
# GIGABYTE B650M D3HP AX rev 1.3 BIOS (gigabyte.com; often blocks non-browser clients)

class GigabyteBios(Adapter):
    name = "gigabyte-bios"
    url = GIGABYTE_D3HP_R13
    fixture = "gigabyte-b650m-d3hp-ax-r13.html"
    # A BIOS version followed closely by its release date, as in the support page's BIOS list.
    _ROW = re.compile(r"\b(F[0-9]{1,3}[a-z]?)\b.{0,160}?\b([0-9]{4})/([0-9]{2})/([0-9]{2})\b", re.S)

    def entries(self):
        return [_entry(
            "gigabyte-b650m-d3hp-ax-r13-bios", "GIGABYTE", "B650M D3HP AX (rev. 1.3) BIOS",
            {"categories": ["bios"], "board": {"vendor": "GIGABYTE", "product": "B650M D3HP AX", "revisions": ["1.3"]}},
            "gigabyte-bios", GIGABYTE_D3HP_R13 + "#support-dl-bios",
        )]

    def parse(self, html):
        p = Page(html)
        text = p.text
        at = text.find("BIOS")
        if at < 0:
            raise AdapterError("no BIOS section")
        found = []
        for m in self._ROW.finditer(text, at):
            v = m.group(1)
            if versions.is_valid("gigabyte-bios", v):
                found.append((v, _date(m.group(2), m.group(3), m.group(4))))
        if not found:
            raise AdapterError("no BIOS version with a release date (the list may be loaded by JavaScript)")
        best = found[0]
        for v in found[1:]:
            if versions.compare_latest("gigabyte-bios", v[0], best[0]) == 1:
                best = v
        return {"gigabyte-b650m-d3hp-ax-r13-bios": {"version": best[0], "release_date": best[1], "release_notes_url": None}}


ADAPTERS = [IntelArc(), AmdChipset(), RealtekLan(), GigabyteBios()]
