"""Per-vendor version schemes, the same rules as the app (src/drivers/catalog/version.rs).

Both implementations are checked against tests/version_vectors.json, so CI's no-downgrade rule and
the app's "newer available" decision cannot drift apart.
"""

import re

SCHEMES = ("intel-gfx", "amd-chipset", "realtek-lan", "numeric", "gigabyte-bios")

_INTEL = re.compile(r"^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{3})\.([0-9]{3,5})$")
_AMD = re.compile(r"^([0-9]{1,2})\.([0-9]{2})\.([0-9]{2})\.([0-9]{3,4})$")
_BIOS = re.compile(r"^[Ff]([0-9]{1,3})([A-Za-z]?)$")


def _numeric(s, lo, hi):
    parts = s.split(".")
    if not lo <= len(parts) <= hi:
        return None
    out = []
    for p in parts:
        if not (1 <= len(p) <= 9) or not p.isascii() or not p.isdigit():
            return None
        out.append(int(p))
    return out


def _bios_key(s):
    m = _BIOS.fullmatch(s)
    if not m:
        return None
    suffix = m.group(2).lower()
    return (0, [int(m.group(1)), (ord(suffix) - ord("a") + 1) if suffix else 0])


def latest_key(scheme, s):
    """Key of a version as a vendor page writes it (strict). (family, parts) or None."""
    if not isinstance(s, str):
        return None
    if scheme == "intel-gfx":
        m = _INTEL.fullmatch(s)
        return (0, [int(m.group(3)), int(m.group(4))]) if m else None
    if scheme == "amd-chipset":
        m = _AMD.fullmatch(s)
        return (0, [int(g) for g in m.groups()]) if m else None
    if scheme == "realtek-lan":
        p = _numeric(s, 3, 3)
        return (p[0], [p[1]]) if p and p[0] in (10, 11) else None
    if scheme == "numeric":
        p = _numeric(s, 2, 5)
        return (0, p) if p else None
    if scheme == "gigabyte-bios":
        return _bios_key(s)
    return None


def installed_key(scheme, s):
    """Key of the version Windows reports for the installed driver."""
    if not isinstance(s, str):
        return None
    s = s.strip()
    if scheme == "intel-gfx":
        return latest_key(scheme, s)
    if scheme == "amd-chipset":
        p = _numeric(s, 4, 4)
        return (0, p) if p else None
    if scheme == "realtek-lan":
        p = _numeric(s, 4, 4)
        if not p:
            return None
        if p[0] == 10:
            return (10, [p[1]])
        if 1100 <= p[0] <= 1199:
            return (11, [p[1]])
        return None
    if scheme == "numeric":
        p = _numeric(s, 2, 5)
        return (0, p) if p else None
    if scheme == "gigabyte-bios":
        return _bios_key(s)
    return None


def _cmp(a, b):
    if a is None or b is None or a[0] != b[0]:
        return None
    n = max(len(a[1]), len(b[1]))
    x = a[1] + [0] * (n - len(a[1]))
    y = b[1] + [0] * (n - len(b[1]))
    return (x > y) - (x < y)


def is_valid(scheme, s):
    return latest_key(scheme, s) is not None


def compare(scheme, installed, latest):
    """1 if `latest` is newer than `installed`, 0 if equal, -1 if older, None if not comparable."""
    return _cmp(latest_key(scheme, latest), installed_key(scheme, installed))


def compare_latest(scheme, a, b):
    """Orders two catalog versions of one scheme (None if not comparable)."""
    return _cmp(latest_key(scheme, a), latest_key(scheme, b))
