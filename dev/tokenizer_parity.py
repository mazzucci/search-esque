#!/usr/bin/env python3
"""Does the profile's tokenizer split exactly where SQLite's FTS5 `unicode61` does?

Runs every text value of the corpus (and a list of awkward strings) through `esque tokens` and
through an FTS5 table with the store's tokenizer options, and compares the sets of distinct
tokens. Python's own sqlite3 module provides FTS5.

    cargo build --release && python3 dev/tokenizer_parity.py [fixtures/<corpus>]
"""
import json
import sqlite3
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ESQUE = ROOT / "target" / "release" / "esque"
TOKENIZE = "unicode61 remove_diacritics 0"

AWKWARD = [
    "Facet-counts, v1.2 (Chat)", "a+b $5 \U0001F33Fx", "Capture d’écran", "résumé Café",
    "50%_off", "doesn't", "x_y", "図書館の夜", "tab\there new\nline", "Ångström ıİ",
    "ß STRASSE", "Σίσυφος", "АБВ абв", "ภาษาไทย",
    "हिन्दी", "zero​width", "é́ double", "\U0001F600\U0001F601 emoji only", "", "   ",
]


def fts5_tokens(con, text):
    """The store's own SQLite (bundled into esque) decides; Python's sqlite3 is only a fallback."""
    if con is None:
        p = subprocess.run([str(ESQUE), "fts-tokens", text], capture_output=True, text=True)
        return json.loads(p.stdout)
    con.execute("DELETE FROM t")
    con.execute("INSERT INTO t VALUES (?)", (text,))
    return sorted(r[0] for r in con.execute("SELECT term FROM v"))


def esque_tokens(texts):
    out = []
    for text in texts:
        p = subprocess.run([str(ESQUE), "tokens", text], capture_output=True, text=True)
        out.append(sorted(set(json.loads(p.stdout))))
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    corpus = Path(args[0]) if args else next(ROOT.glob("fixtures/*/docs.json")).parent
    mapping = json.load(open(corpus / "mapping.json"))["properties"]
    docs = json.load(open(corpus / "docs.json"))
    text_fields = [n for n, f in mapping.items() if f["type"] == "text"]
    texts = list(AWKWARD)
    for d in docs:
        for f in text_fields:
            v = d.get(f)
            texts += v if isinstance(v, list) else ([v] if isinstance(v, str) else [])
    con = None
    if "--python-sqlite" in sys.argv:
        con = sqlite3.connect(":memory:")
        con.execute(f"CREATE VIRTUAL TABLE t USING fts5(x, tokenize='{TOKENIZE}')")
        con.execute("CREATE VIRTUAL TABLE v USING fts5vocab(t, 'row')")
    ours = esque_tokens(texts)
    bad = 0
    for text, mine in zip(texts, ours):
        theirs = fts5_tokens(con, text)
        if mine != theirs:
            bad += 1
            if bad <= 20:
                print(f"DIFFERS {text!r}\n  esque: {mine}\n  fts5:  {theirs}")
    engine = f"python sqlite {sqlite3.sqlite_version}" if con is not None else "the store's bundled SQLite"
    print(f"\n{len(texts) - bad} of {len(texts)} texts tokenize identically against {engine}")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
