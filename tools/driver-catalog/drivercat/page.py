"""A defensive view of a vendor HTML page (stdlib html.parser; no JavaScript, no network).

It keeps only what the adapters read: <meta> name/content pairs, links, <option>s, table rows (with the
nearest heading before each table) and the visible text. Everything is plain strings; nothing from the
page is ever evaluated.
"""

import html.parser
import re

_WS = re.compile(r"\s+")
_SKIP = {"script", "style", "noscript", "template", "svg"}
_BLOCK = {"p", "div", "li", "tr", "td", "th", "h1", "h2", "h3", "h4", "h5", "h6", "br", "section", "article", "option", "label", "span"}


def norm(s):
    return _WS.sub(" ", s or "").strip()


class Page(html.parser.HTMLParser):
    MAX_ITEMS = 20000

    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.meta = {}
        self.links = []      # (href, text)
        self.options = []    # (value, text)
        self.tables = []     # {"heading": str, "rows": [[cell text, ...]]}
        self._text = []
        self._skip = 0
        self._link = None
        self._option = None
        self._heading = None
        self._last_heading = ""
        self._table_stack = []
        self._row = None
        self._cell = None
        self.feed(text)
        self.close()
        self.text = norm(" ".join(self._text))

    # -- helpers
    def _add_text(self, data):
        if len(self._text) < self.MAX_ITEMS:
            self._text.append(data)
        if self._link is not None:
            self._link[1].append(data)
        if self._option is not None:
            self._option[1].append(data)
        if self._heading is not None:
            self._heading.append(data)
        if self._cell is not None:
            self._cell.append(data)

    # -- parser callbacks
    def handle_starttag(self, tag, attrs):
        a = {k: (v or "") for k, v in attrs}
        if tag in _SKIP:
            self._skip += 1
            return
        if self._skip:
            return
        if tag in _BLOCK:
            self._add_text(" ")
        if tag == "meta":
            key = a.get("name") or a.get("property")
            if key and "content" in a and len(self.meta) < 500:
                self.meta.setdefault(key, a["content"])
        elif tag == "a":
            self._link = (a.get("href", ""), [])
        elif tag == "option":
            self._option = (a.get("value", ""), [])
        elif tag in ("h1", "h2", "h3", "h4", "h5", "h6"):
            self._heading = []
        elif tag == "table":
            self._table_stack.append({"heading": self._last_heading, "rows": []})
        elif tag == "tr" and self._table_stack:
            self._row = []
        elif tag in ("td", "th") and self._row is not None:
            self._cell = []

    def handle_endtag(self, tag):
        if tag in _SKIP:
            self._skip = max(0, self._skip - 1)
            return
        if self._skip:
            return
        if tag in _BLOCK:
            self._add_text(" ")
        if tag == "a" and self._link is not None:
            if len(self.links) < self.MAX_ITEMS:
                self.links.append((self._link[0], norm(" ".join(self._link[1]))))
            self._link = None
        elif tag == "option" and self._option is not None:
            self.options.append((self._option[0], norm(" ".join(self._option[1]))))
            self._option = None
        elif tag in ("h1", "h2", "h3", "h4", "h5", "h6") and self._heading is not None:
            self._last_heading = norm(" ".join(self._heading))
            self._heading = None
        elif tag in ("td", "th") and self._cell is not None and self._row is not None:
            self._row.append(norm(" ".join(self._cell)))
            self._cell = None
        elif tag == "tr" and self._row is not None and self._table_stack:
            if self._row and len(self._table_stack[-1]["rows"]) < 2000:
                self._table_stack[-1]["rows"].append(self._row)
            self._row = None
        elif tag == "table" and self._table_stack:
            t = self._table_stack.pop()
            if len(self.tables) < 500:
                self.tables.append(t)

    def handle_data(self, data):
        if not self._skip:
            self._add_text(data)
