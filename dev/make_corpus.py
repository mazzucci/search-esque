#!/usr/bin/env python3
"""Generates the test corpus: an invented library catalogue.

Deterministic (seeded), standard library only, nothing taken from any real dataset. The point
of a designed corpus is that the cases that break compilers are guaranteed to be in it:

- keyword fields with case variants of the same value ("Nordlys Press" and "nordlys press");
- keyword values with the characters LIKE and quoting care about: `%`, `_`, `"`, `'`, `\\`;
- keyword arrays that are empty, missing, or hold a duplicate value;
- text with apostrophes, dotted version numbers, accents, a CJK title and an emoji;
- integers and floats, floats that are whole numbers (10.0), negative numbers, nulls;
- booleans that are true, false or missing;
- dates before 1970 (negative timestamps), on day boundaries, with and without a time part,
  and a date field that is missing in a good share of records;
- a date array (reprint dates, sometimes empty, once holding an unparseable value) and a text
  array (reader notes, so a phrase can be tested not to span two notes);
- a title with a decomposed accent (e + combining acute), a micro sign and a Turkish dotted I.

    python3 dev/make_corpus.py          # writes fixtures/library/docs.json and mapping.json
"""
import json
import random
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "fixtures" / "library"
N = 400
rng = random.Random(20261010)

AUTHORS = [
    "Ada Quill", "Bram Tessler", "Ciela Marrow", "Dov Halloran", "Esme Varga", "Fenn Oduya",
    "Greta Lindqvist", "Hollis Abernathy", "Ines Calloway", "Jorin Pask", "Keziah Blunt",
    "Lior Castellane", "Maud Ferrier", "Nilo Brightwater", "Oona Vesper", "Pell Harrowgate",
    "Quenby Ashdown", "Rafe Montclair", "Sable Winterbourne", "Teodor Vance", "Una Marchetti",
    "Vesna Kowal", "Wren Tallis", "Xavi Delacroix", "Yara Solenne", "Zeno Pembrook",
]
PUBLISHERS = ["Nordlys Press", "nordlys press", "Harbourline", "Tessellate Books", "Quill & Ember", "Meridian House"]
LANGUAGES = ["en", "en", "en", "en", "fr", "de", "es", "pt", "ja"]
FORMATS = ["hardcover", "paperback", "paperback", "ebook", "audiobook"]
GENRES = ["fiction", "mystery", "history", "science", "poetry", "travel", "cooking", "fantasy", "memoir", "essays"]
TAGS = [
    "staff pick", "signed", "first edition", "50%_off", "back\\slash", "quote\"d", "it's rare",
    "large print", "box set", "award winner", "translated", "illustrated",
]
TITLE_WORDS = [
    "Harbour", "Lanterns", "Winter", "Atlas", "Garden", "Clockwork", "Meridian", "Salt", "Orchard",
    "Shadow", "River", "Compass", "Ember", "Quiet", "Glass", "Mountain", "Letters", "Season",
]
SPECIAL_TITLES = [
    "The Baker Who Doesn't Sleep",
    "Field Notes v2.0 for the Night Harbour",
    "Café Lanterns and Naïve Winters",
    "図書館の夜",
    "A Garden of Compasses 🌿",
    "Harbour, Harbour: Letters from the River",
    "What's Left of the Orchard",
    "Release 1.41.0 and Other Clockwork Tales",
    "Caf\u0065\u0301 Noir at 5\u00b5s",
    "\u0130stanbul Letters, \u0131\u015f\u0131k",
]
NOTES = [
    "a quiet read for a winter evening", "the ending doesn't land", "maps are beautiful", "salt bread recipe works",
    "pages fall out of the box set", "harbour scenes are the best part", "too many letters, too few lanterns",
    "the clockwork plot ticks along", "v2.0 fixed the typos", "great for the garden bench",
]
SUMMARY_BITS = [
    "a slow walk through the harbour at dusk", "letters that were never sent", "the clockwork heart of a lighthouse",
    "a recipe for salt bread", "an atlas of invented rivers", "a winter that doesn't end",
    "the quiet garden behind the station", "field notes from the mountain path", "a season of lanterns",
    "the compass that pointed home", "ember and glass", "a mystery in three letters",
]


def iso(d: datetime, with_time: bool) -> str:
    return d.strftime("%Y-%m-%dT%H:%M:%SZ") if with_time else d.strftime("%Y-%m-%d")


def date_between(start: datetime, end: datetime, with_time: bool) -> str:
    seconds = int((end - start).total_seconds())
    d = start + timedelta(seconds=rng.randrange(seconds))
    return iso(d, with_time)


def make_doc(i: int) -> dict:
    utc = timezone.utc
    title = SPECIAL_TITLES[i] if i < len(SPECIAL_TITLES) else " ".join(rng.sample(TITLE_WORDS, rng.choice([2, 3, 3, 4])))
    summary = "; ".join(rng.sample(SUMMARY_BITS, 3)).capitalize() + "."
    genres = rng.sample(GENRES, rng.choice([0, 1, 1, 2, 2, 3, 4]))
    if i % 25 == 0 and genres:
        genres = genres + [genres[0]]          # a duplicate value, on purpose
    tags = rng.sample(TAGS, rng.choice([0, 0, 1, 2, 3]))
    price = None if i % 17 == 0 else (float(rng.randrange(5, 60)) if i % 9 == 0 else round(rng.uniform(4.5, 89.99), 2))
    rating = None if i % 7 == 0 else round(rng.uniform(1.0, 5.0), 1)
    published = date_between(datetime(1850, 1, 1, tzinfo=utc), datetime(2026, 10, 1, tzinfo=utc), with_time=(i % 3 == 0))
    if i % 40 == 0:
        published = "1969-12-31T23:59:59Z"     # the last second before the epoch
    if i % 40 == 1:
        published = "1970-01-01"
    added = date_between(datetime(2024, 1, 1, tzinfo=utc), datetime(2026, 10, 10, tzinfo=utc), with_time=True)
    reprints = [date_between(datetime(1900, 1, 1, tzinfo=utc), datetime(2026, 1, 1, tzinfo=utc), with_time=(k % 2 == 0)) for k in range(rng.choice([0, 0, 1, 2, 3]))]
    if i % 50 == 7:
        reprints = ["not a date"]                 # an array holding only an unparseable value
    notes = rng.sample(NOTES, rng.choice([0, 1, 2, 3]))
    doc = {
        "_id": f"b{i + 1:04d}",
        "title": title,
        "summary": summary,
        "author": rng.choice(AUTHORS),
        "publisher": rng.choice(PUBLISHERS),
        "language": rng.choice(LANGUAGES),
        "format": rng.choice(FORMATS),
        "genres": genres if i % 23 != 5 else None,          # mostly a list, sometimes missing
        "tags": tags,
        "pages": None if i % 13 == 0 else rng.randrange(40, 1200),
        "price": price,
        "rating": rating,
        "balance": round(rng.uniform(-40.0, 60.0), 2),      # negative numbers for histograms
        "copies": rng.randrange(0, 13),
        "available": None if i % 11 == 0 else rng.random() < 0.7,
        "published_at": published,
        "added_at": added,
        "last_borrowed_at": None if rng.random() < 0.4 else date_between(datetime(2025, 1, 1, tzinfo=utc), datetime(2026, 10, 10, tzinfo=utc), with_time=True),
        "isbn": f"978-1-{rng.randrange(100000, 999999)}-{i:03d}",
        "reprinted_at": reprints,
        "notes": notes,
    }
    return doc


MAPPING = {"properties": {
    "title": {"type": "text"},
    "summary": {"type": "text"},
    "author": {"type": "keyword"},
    "publisher": {"type": "keyword"},
    "language": {"type": "keyword"},
    "format": {"type": "keyword"},
    "genres": {"type": "keyword", "array": True},
    "tags": {"type": "keyword", "array": True},
    "pages": {"type": "long"},
    "price": {"type": "double"},
    "rating": {"type": "double"},
    "balance": {"type": "double"},
    "copies": {"type": "long"},
    "available": {"type": "boolean"},
    "published_at": {"type": "date"},
    "added_at": {"type": "date"},
    "last_borrowed_at": {"type": "date"},
    "isbn": {"type": "keyword"},
    "reprinted_at": {"type": "date", "array": True},
    "notes": {"type": "text", "array": True},
}}


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    docs = [make_doc(i) for i in range(N)]
    (ROOT / "docs.json").write_text(json.dumps(docs, indent=0, ensure_ascii=False) + "\n")
    (ROOT / "mapping.json").write_text(json.dumps(MAPPING, indent=1) + "\n")
    print(f"{len(docs)} records, {len(MAPPING['properties'])} fields -> {ROOT}")


if __name__ == "__main__":
    main()
