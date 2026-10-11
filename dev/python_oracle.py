#!/usr/bin/env python3
"""An independent seed oracle for the fixtures.

Computes, in plain Python and without reading the Rust code, what each fixture request must
answer over fixtures/library/docs.json (the generated corpus, see dev/make_corpus.py), and
writes the fixture files. It covers only the constructs the fixtures use. Once the Rust
evaluator agrees with these files, the evaluator is the reference and this script is history;
keep it for re-seeding if the corpus changes.

    python3 dev/python_oracle.py
"""
import json
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "fixtures" / "library"
DOCS = json.load(open(ROOT / "docs.json"))
MAPPING = json.load(open(ROOT / "mapping.json"))["properties"]
MS_DAY = 86_400_000


def ftype(field):
    t = MAPPING[field]["type"]
    return {"long": "number", "double": "number", "boolean": "boolean"}.get(t, t)


def is_array(field):
    return MAPPING[field].get("array", False)


def parse_iso(s):
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    d = datetime.fromisoformat(s)
    if d.tzinfo is None:
        d = d.replace(tzinfo=timezone.utc)
    return int(d.timestamp() * 1000)


def values(doc, field):
    """The field's values that fit its type; anything else is empty (as both engines treat it)."""
    v = doc.get(field)
    if v is None:
        return []
    if isinstance(v, list) and not is_array(field):
        return []
    items = [x for x in v if x is not None] if isinstance(v, list) else [v]
    t = ftype(field)
    if t == "date":
        out = []
        for x in items:
            try:
                out.append(parse_iso(x))
            except (ValueError, TypeError, AttributeError):
                pass
        return out
    if t == "number":
        return [x for x in items if isinstance(x, (int, float)) and not isinstance(x, bool)]
    if t == "boolean":
        return [x for x in items if isinstance(x, bool)]
    return [x for x in items if isinstance(x, str)]


def fold(s):
    # the profile's case fold is Unicode simple folding; the one case this corpus has is µ to μ
    return s.lower().replace("\u00b5", "\u03bc")


def tokens(s):
    # separators as FTS5 unicode61: everything that is not a letter, digit or combining mark;
    # good enough for this corpus, the Rust table is the measured truth
    return [fold(t) for t in re.findall(r"[\w\u0300-\u036f]+", s.replace("_", " ")) if t]


def norm(field, v):
    t = ftype(field)
    if t == "date":
        return parse_iso(v)
    if t in ("keyword", "text") and isinstance(v, str):
        return v.lower()
    return v


def as_list(v):
    if v is None:
        return []
    return v if isinstance(v, list) else [v]


def matches(q, doc):
    (kind, body), = q.items()
    if kind == "match_all":
        return True
    if kind == "bool":
        must = as_list(body.get("must"))
        filt = as_list(body.get("filter"))
        must_not = as_list(body.get("must_not"))
        should = as_list(body.get("should"))
        if not all(matches(c, doc) for c in must + filt):
            return False
        if any(matches(c, doc) for c in must_not):
            return False
        if not should:
            return True
        need = max(body.get("minimum_should_match", 1), 1) if not must and not filt else body.get("minimum_should_match", 0)
        return sum(matches(c, doc) for c in should) >= need
    if kind == "term":
        (field, spec), = body.items()
        want = spec["value"] if isinstance(spec, dict) else spec
        if ftype(field) == "date" and len(want) == 10:
            lo = parse_iso(want)
            return any(lo <= v <= lo + MS_DAY - 1 for v in values(doc, field))
        want = norm(field, want)
        return any((norm(field, v) if ftype(field) != "date" else v) == want for v in values(doc, field))
    if kind == "terms":
        (field, vals), = body.items()
        if ftype(field) == "date":
            vs = values(doc, field)
            for w in vals:
                lo = parse_iso(w)
                hi = lo + MS_DAY - 1 if len(w) == 10 else lo
                if any(lo <= v <= hi for v in vs):
                    return True
            return False
        wanted = {norm(field, v) for v in vals}
        return any(norm(field, v) in wanted for v in values(doc, field))
    if kind == "range":
        (field, b), = body.items()
        is_date = ftype(field) == "date"

        def conv(x, up=False):
            # Elasticsearch rounds a date-only bound up to the end of the day for gt and lte
            return (parse_iso(x) + (MS_DAY - 1 if up and len(x) == 10 else 0)) if is_date else x

        for v in values(doc, field):
            ok = True
            if "gte" in b: ok &= v >= conv(b["gte"])
            if "gt" in b: ok &= v > conv(b["gt"], True)
            if "lte" in b: ok &= v <= conv(b["lte"], True)
            if "lt" in b: ok &= v < conv(b["lt"])
            if ok:
                return True
        return False
    if kind == "exists":
        return bool(values(doc, body["field"]))
    if kind == "match":
        (field, spec), = body.items()
        text, op = (spec["query"], spec.get("operator", "or")) if isinstance(spec, dict) else (spec, "or")
        want = tokens(text)
        have = [t for v in values(doc, field) for t in tokens(v)]
        return (all if op.lower() == "and" else any)(w in have for w in want)
    if kind == "match_phrase":
        (field, spec), = body.items()
        want = tokens(spec["query"] if isinstance(spec, dict) else spec)
        for v in values(doc, field):
            have = tokens(v)
            if any(have[i:i + len(want)] == want for i in range(len(have) - len(want) + 1)):
                return True
        return False
    if kind == "prefix":
        (field, spec), = body.items()
        want = (spec["value"] if isinstance(spec, dict) else spec).lower()
        if ftype(field) == "text":
            return any(t.startswith(want) for v in values(doc, field) for t in tokens(v))
        return any(v.lower().startswith(want) for v in values(doc, field))
    raise ValueError(kind)


def es_double(n):
    return f"{int(n)}.0" if float(n).is_integer() else repr(float(n))


def num(v):
    return int(v) if float(v).is_integer() else v


def iso(ms):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S.") + f"{d.microsecond // 1000:03d}Z"


def bucket_start(ms, iv):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    if iv == "year":
        d = datetime(d.year, 1, 1, tzinfo=timezone.utc)
    elif iv == "month":
        d = datetime(d.year, d.month, 1, tzinfo=timezone.utc)
    else:
        d = datetime(d.year, d.month, d.day, tzinfo=timezone.utc)
    return int(d.timestamp() * 1000)


def next_bucket(ms, iv):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    if iv == "year":
        n = datetime(d.year + 1, 1, 1, tzinfo=timezone.utc)
    elif iv == "month":
        y, m = (d.year + 1, 1) if d.month == 12 else (d.year, d.month + 1)
        n = datetime(y, m, 1, tzinfo=timezone.utc)
    else:
        n = d + timedelta(days=1)
    return int(n.timestamp() * 1000)


def key_sort(k):
    if isinstance(k, bool):
        return (0, k)
    if isinstance(k, (int, float)):
        return (1, k)
    return (2, k)


def aggregate(spec, docs):
    (kind, body), = spec.items()
    field = body["field"]
    t = ftype(field)
    if kind == "terms":
        counts = {}
        for d in docs:
            for k in set(values(d, field)):
                counts[k] = counts.get(k, 0) + 1
        order = body.get("order", {"_count": "desc"})
        (okey, odir), = order.items()
        items = list(counts.items())
        if okey == "_count":
            items.sort(key=lambda kv: ((-kv[1] if odir == "desc" else kv[1]), key_sort(kv[0])))
        else:
            items.sort(key=lambda kv: key_sort(kv[0]), reverse=(odir == "desc"))
        size = body.get("size", 10)
        shown, rest = items[:size], items[size:]
        buckets = []
        for k, c in shown:
            if t == "boolean":
                buckets.append({"key": 1 if k else 0, "key_as_string": str(k).lower(), "doc_count": c})
            elif t == "date":
                buckets.append({"key": k, "key_as_string": iso(k), "doc_count": c})
            else:
                buckets.append({"key": num(k) if t == "number" else k, "doc_count": c})
        return {"doc_count_error_upper_bound": 0, "sum_other_doc_count": sum(c for _, c in rest), "buckets": buckets}
    if kind == "range":
        out = []
        ordered = sorted(body["ranges"], key=lambda r: (r.get("from", float("-inf")), r.get("to", float("inf"))))
        for r in ordered:
            lo, hi = r.get("from"), r.get("to")
            n = sum(1 for d in docs if any((lo is None or v >= lo) and (hi is None or v < hi) for v in values(d, field)))
            b = {"key": r.get("key", f"{'*' if lo is None else es_double(lo)}-{'*' if hi is None else es_double(hi)}")}
            if lo is not None: b["from"] = float(lo)
            if hi is not None: b["to"] = float(hi)
            b["doc_count"] = n
            out.append(b)
        return {"buckets": out}
    if kind == "histogram":
        iv = body["interval"]
        counts = {}
        for d in docs:
            for b in {int(v // iv) for v in values(d, field)}:
                counts[b] = counts.get(b, 0) + 1
        out = []
        if counts:
            for b in range(min(counts), max(counts) + 1):
                c = counts.get(b, 0)
                if c >= body.get("min_doc_count", 0):
                    out.append({"key": num(b * iv), "doc_count": c})
        return {"buckets": out}
    if kind == "date_histogram":
        iv = {"1M": "month", "1y": "year", "1d": "day"}.get(body["calendar_interval"], body["calendar_interval"])
        counts = {}
        for d in docs:
            for b in {bucket_start(v, iv) for v in values(d, field)}:
                counts[b] = counts.get(b, 0) + 1
        out = []
        if counts:
            b = min(counts)
            while b <= max(counts):
                c = counts.get(b, 0)
                if c >= body.get("min_doc_count", 0):
                    out.append({"key_as_string": iso(b), "key": b, "doc_count": c})
                b = next_bucket(b, iv)
        return {"buckets": out}
    if kind == "value_count":
        # keyword arrays: Elasticsearch's sorted-set doc values count a repeated value once per record
        if is_array(field) and t == "keyword":
            return {"value": sum(len(set(values(d, field))) for d in docs)}
        return {"value": sum(len(values(d, field)) for d in docs)}
    if kind == "cardinality":
        return {"value": len({v for d in docs for v in values(d, field)})}
    nums = [v for d in docs for v in values(d, field)]
    n = len(nums)
    s = sum(nums)
    mn, mx = (min(nums), max(nums)) if nums else (None, None)
    avg = s / n if n else None
    if kind == "avg": return {"value": avg}
    if kind == "min": return {"value": None if mn is None else float(mn)}
    if kind == "max": return {"value": None if mx is None else float(mx)}
    if kind == "sum": return {"value": float(s)}
    if kind == "stats":
        return {"count": n, "min": None if mn is None else float(mn), "max": None if mx is None else float(mx), "avg": avg, "sum": float(s)}
    raise ValueError(kind)


def run(request):
    query = request.get("query", {"match_all": {}})
    matched = [d for d in DOCS if matches(query, d)]
    hits = [d for d in matched if matches(request["post_filter"], d)] if "post_filter" in request else list(matched)
    sorts = request.get("sort", [])
    sorts = sorts if isinstance(sorts, list) else [sorts]

    def sort_key(d):
        parts = []
        for s in sorts:
            if isinstance(s, str):
                field, order = s, "asc"
            else:
                (field, spec), = s.items()
                order = spec if isinstance(spec, str) else spec.get("order", "asc")
            vs = values(d, field)
            if not vs:
                parts.append((1, 0))
            else:
                v = vs[0]
                if isinstance(v, str):
                    assert order == "asc", "the oracle only sorts strings ascending"
                    parts.append((0, v))
                else:
                    parts.append((0, -v if order == "desc" else v))
        return parts + [d["_id"]]

    hits.sort(key=sort_key)
    size, frm = request.get("size", 10), request.get("from", 0)
    page = hits[frm:frm + size]
    out = {"total": len(hits), "hit_ids": [d["_id"] for d in page]}
    aggs = request.get("aggs") or request.get("aggregations") or {}
    if aggs:
        out["aggregations"] = {name: aggregate(spec, matched) for name, spec in aggs.items()}
    src = request.get("_source", True)
    if page and src is not True:
        if src is False:
            out["source_keys"] = []
        else:
            out["source_keys"] = sorted(k for k in page[0] if k in (src if isinstance(src, list) else [src]))
    return out


FIXTURES = [
    ("match_all", {"query": {"match_all": {}}, "size": 3}),
    ("format_terms", {"size": 0, "aggs": {"by_format": {"terms": {"field": "format"}}}}),
    ("must_not_term_language", {"size": 0, "query": {"bool": {"must_not": {"term": {"language": "en"}}}}}),
    ("must_not_term_available_includes_missing", {"size": 0, "query": {"bool": {"must_not": {"term": {"available": True}}}}}),
    ("must_not_range_published_since_1970", {"size": 0, "query": {"bool": {"must_not": {"range": {"published_at": {"gte": "1970-01-01"}}}}}}),
    ("available_mysteries_by_genre", {
        "query": {"bool": {"filter": [{"term": {"available": True}}, {"terms": {"genres": ["mystery"]}}]}},
        "aggs": {"genres": {"terms": {"field": "genres", "size": 5}}},
        "sort": [{"rating": "desc"}], "size": 10}),
    ("long_books_stats", {"size": 0, "query": {"range": {"pages": {"gte": 500}}},
        "aggs": {"p": {"stats": {"field": "pages"}}, "price": {"avg": {"field": "price"}}, "balance": {"max": {"field": "balance"}}, "copies": {"sum": {"field": "copies"}}}}),
    ("ebooks_added_per_month", {"size": 0, "query": {"term": {"format": "ebook"}},
        "aggs": {"per_month": {"date_histogram": {"field": "added_at", "calendar_interval": "month"}}}}),
    ("published_per_year_before_1970", {"size": 0, "query": {"range": {"published_at": {"lt": "1970-01-01"}}},
        "aggs": {"per_year": {"date_histogram": {"field": "published_at", "calendar_interval": "year", "min_doc_count": 1}}}}),
    ("published_per_day_across_the_epoch", {"size": 0, "query": {"range": {"published_at": {"gte": "1969-12-30", "lt": "1970-01-03"}}},
        "aggs": {"per_day": {"date_histogram": {"field": "published_at", "calendar_interval": "day"}}}}),
    ("match_title_harbour", {"size": 0, "query": {"match": {"title": "harbour"}}}),
    ("match_title_and", {"size": 0, "query": {"match": {"title": {"query": "harbour letters", "operator": "and"}}}}),
    ("match_title_or", {"size": 0, "query": {"match": {"title": {"query": "harbour letters", "operator": "or"}}}}),
    ("match_phrase_summary", {"size": 0, "query": {"match_phrase": {"summary": "salt bread"}}}),
    ("match_title_accent", {"size": 0, "query": {"match": {"title": "café"}}}),
    ("match_title_cjk", {"size": 0, "query": {"match": {"title": "図書館の夜"}}}),
    ("exists_last_borrowed", {"size": 0, "query": {"exists": {"field": "last_borrowed_at"}}}),
    ("not_exists_last_borrowed", {"size": 0, "query": {"bool": {"must_not": {"exists": {"field": "last_borrowed_at"}}}}}),
    ("exists_genres_empty_is_missing", {"size": 0, "query": {"exists": {"field": "genres"}}}),
    ("copies_histogram", {"size": 0, "aggs": {"h": {"histogram": {"field": "copies", "interval": 3}}}}),
    ("balance_histogram_negative", {"size": 0, "aggs": {"h": {"histogram": {"field": "balance", "interval": 20}}}}),
    ("stock_delta_histogram_negative_integers", {"size": 0, "aggs": {"h": {"histogram": {"field": "stock_delta", "interval": 10}}}}),
    # 9e18 fits a long (Elasticsearch refuses bounds outside the field's numeric type; the profile compares as doubles),
    # and -1e300 wraps if a driver binds it as an integer
    ("range_with_huge_bounds", {"size": 0, "query": {"bool": {"filter": [{"range": {"pages": {"lte": 9e18}}}, {"range": {"balance": {"gte": -1e300}}}]}}}),
    ("price_ranges", {"size": 0, "aggs": {"r": {"range": {"field": "price", "ranges": [{"to": 10}, {"from": 10, "to": 30}, {"from": 30, "key": "pricey"}]}}}}),
    ("range_agg_ordered_by_from", {"size": 0, "aggs": {"r": {"range": {"field": "balance", "ranges": [{"from": 0, "to": 20}, {"from": 20}, {"to": 0}]}}}}),
    ("should_fr_or_de", {"size": 0, "query": {"bool": {"should": [{"term": {"language": "fr"}}, {"term": {"language": "de"}}]}}}),
    ("should_with_must_is_optional", {"size": 0, "query": {"bool": {"must": {"term": {"format": "paperback"}}, "should": [{"term": {"author": "nobody-here"}}]}}}),
    ("should_with_must_msm1", {"size": 0, "query": {"bool": {"must": {"term": {"format": "paperback"}}, "should": [{"term": {"language": "fr"}}], "minimum_should_match": 1}}}),
    ("msm_zero_should_only_needs_one", {"size": 0, "query": {"bool": {"should": [{"term": {"language": "fr"}}], "minimum_should_match": 0}}}),
    ("prefix_author_case_folded", {"size": 0, "query": {"prefix": {"author": "ad"}}}),
    ("prefix_tag_percent_is_literal", {"size": 0, "query": {"prefix": {"tags": "50%"}}}),
    ("prefix_tag_underscore_is_literal", {"size": 0, "query": {"prefix": {"tags": "50%_o"}}}),
    ("prefix_title_token", {"size": 0, "query": {"prefix": {"title": "lan"}}}),
    ("term_tag_with_quote", {"size": 0, "query": {"term": {"tags": "quote\"d"}}}),
    ("term_tag_with_backslash", {"size": 0, "query": {"term": {"tags": "back\\slash"}}}),
    ("term_genre_counts_duplicates_once", {"size": 0, "query": {"term": {"genres": "poetry"}}, "aggs": {"g": {"terms": {"field": "genres", "size": 20}}}}),
    ("term_price_whole_float", {"size": 0, "query": {"term": {"price": 42}}}),
    ("sort_price_desc_missing_last", {"query": {"match_all": {}}, "sort": [{"price": {"order": "desc"}}], "size": 5, "_source": ["title", "price"]}),
    ("sort_published_asc_from", {"query": {"term": {"format": "hardcover"}}, "sort": ["published_at"], "size": 4, "from": 2}),
    ("sort_rating_asc_missing_last_at_end", {"query": {"match_all": {}}, "sort": [{"rating": "asc"}], "size": 3, "from": 397, "_source": ["rating"]}),
    ("term_case_insensitive", {"size": 0, "query": {"term": {"publisher": "NORDLYS PRESS"}}}),
    ("publisher_terms_keep_case_variants_apart", {"size": 0, "aggs": {"p": {"terms": {"field": "publisher"}}}}),
    ("cardinality_and_value_count", {"size": 0, "aggs": {"authors": {"cardinality": {"field": "author"}}, "genre_values": {"value_count": {"field": "genres"}}, "tag_values": {"value_count": {"field": "tags"}}, "borrowed": {"value_count": {"field": "last_borrowed_at"}}, "prices": {"cardinality": {"field": "price"}}}}),
    ("source_fields", {"query": {"match_all": {}}, "size": 1, "_source": ["title", "price"]}),
    ("source_false", {"query": {"match_all": {}}, "size": 2, "_source": False}),
    ("post_filter_keeps_aggs", {"size": 0, "aggs": {"by_format": {"terms": {"field": "format"}}}, "post_filter": {"term": {"format": "ebook"}}}),
    ("available_terms_boolean", {"size": 0, "aggs": {"a": {"terms": {"field": "available"}}}}),
    ("author_terms_key_asc", {"size": 0, "aggs": {"a": {"terms": {"field": "author", "size": 3, "order": {"_key": "asc"}}}}}),
    ("language_count_asc", {"size": 0, "aggs": {"l": {"terms": {"field": "language", "order": {"_count": "asc"}}}}}),
    ("price_terms_double_keys", {"size": 0, "aggs": {"p": {"terms": {"field": "price", "size": 5}}}}),
    ("term_date_exact", {"size": 0, "query": {"term": {"published_at": "1969-12-31T23:59:59Z"}}}),
    ("range_date_window", {"size": 0, "query": {"range": {"added_at": {"gte": "2026-01-01", "lt": "2026-07-01"}}},
        "aggs": {"by_format": {"terms": {"field": "format"}}}}),
    ("range_date_lte_rounds_up", {"size": 0, "query": {"range": {"added_at": {"lte": "2026-06-30"}}}}),
    ("range_date_gt_rounds_up", {"size": 0, "query": {"range": {"added_at": {"gt": "2026-06-30"}}}}),
    ("page_past_the_end", {"query": {"match_all": {}}, "size": 5, "from": 398}),
    # the date array and the text array
    ("reprinted_since_2000", {"size": 0, "query": {"range": {"reprinted_at": {"gte": "2000-01-01"}}}}),
    ("reprinted_per_year", {"size": 0, "aggs": {"y": {"date_histogram": {"field": "reprinted_at", "calendar_interval": "year", "min_doc_count": 1}}}}),
    ("exists_reprinted_unparseable_is_empty", {"size": 0, "query": {"exists": {"field": "reprinted_at"}}}),
    ("reprint_values_and_distinct", {"size": 0, "aggs": {"n": {"value_count": {"field": "reprinted_at"}}, "d": {"cardinality": {"field": "reprinted_at"}}}}),
    ("term_date_day_means_whole_day", {"size": 0, "query": {"term": {"added_at": "2026-03-15"}}}),
    ("terms_date_days_mean_whole_days", {"size": 0, "query": {"terms": {"added_at": ["2026-03-15", "2026-03-16", "2025-11-02T10:00:00Z"]}}}),
    ("match_notes_word", {"size": 0, "query": {"match": {"notes": "harbour"}}}),
    ("match_phrase_notes_within_one_note", {"size": 0, "query": {"match_phrase": {"notes": "salt bread recipe"}}}),
    ("match_phrase_notes_never_spans_two", {"size": 0, "query": {"match_phrase": {"notes": "evening the ending"}}}),
    ("match_title_decomposed_accent", {"size": 0, "query": {"match": {"title": "cafe\u0301"}}}),
    ("match_title_micro_sign_folds_to_mu", {"size": 0, "query": {"match": {"title": "5\u03bcs"}}}),
    ("prefix_notes_token", {"size": 0, "query": {"prefix": {"notes": "lant"}}}),
    # Tokenizer divergences from Elasticsearch's standard analyzer, pinned to the profile's answer
    ("match_apostrophe_token", {"size": 0, "query": {"match": {"title": "doesn"}}}),
    ("match_dotted_token", {"size": 0, "query": {"match": {"title": "v2"}}}),
]

ERRORS = [
    ("err_wildcard", {"query": {"bool": {"filter": [{"term": {"format": "ebook"}}, {"wildcard": {"title": "har*"}}]}}}, "query.bool.filter[1].wildcard"),
    ("err_unknown_field", {"query": {"term": {"autor": "Ada Quill"}}}, "query.term.autor"),
    ("err_quoted_number", {"query": {"range": {"pages": {"gte": "500"}}}}, "query.range.pages.gte"),
    ("err_month_not_date", {"aggs": {"x": {"terms": {"field": "format"}}}, "query": {"range": {"added_at": {"gte": "2026-07"}}}}, "query.range.added_at.gte"),
    ("err_terms_on_text", {"aggs": {"by": {"terms": {"field": "summary"}}}}, "aggs.by.terms.field"),
    ("err_not_elasticsearch", {"filters": [{"field": "format", "eq": "ebook"}]}, "$.filters"),
    ("err_sub_aggregation", {"aggs": {"by": {"terms": {"field": "format"}, "aggs": {"c": {"avg": {"field": "price"}}}}}}, "aggs.by.aggs"),
    ("err_sort_array", {"sort": ["tags"]}, "sort[0].tags"),
    ("err_match_on_keyword", {"query": {"match": {"author": "Ada"}}}, "query.match.author"),
    ("err_size_too_big", {"size": 20000}, "size"),
    ("err_result_window", {"size": 100, "from": 9950}, "from"),
    # refused at run time, not at parse time: the path names the aggregation
    ("err_histogram_too_many_buckets", {"size": 0, "aggs": {"h": {"histogram": {"field": "balance", "interval": 0.000001}}}}, "aggs.h"),
    ("err_double_underscore_mapping_name_is_refused_elsewhere", {"query": {"term": {"a__b": 1}}}, "query.term.a__b"),
]


def main():
    qdir = ROOT / "queries"
    qdir.mkdir(exist_ok=True)
    for old in qdir.glob("*.json"):
        old.unlink()
    for i, (name, request) in enumerate(FIXTURES, 1):
        expect = run(request)
        if "sort" not in request and request.get("size", 10) == 0:
            expect.pop("hit_ids", None)
        fx = {"name": name, "request": request, "expect": expect}
        (qdir / f"{i:02d}-{name}.json").write_text(json.dumps(fx, indent=1, ensure_ascii=False) + "\n")
        print(f"{i:02d} {name}: total {expect['total']}")
    for j, (name, request, path) in enumerate(ERRORS, len(FIXTURES) + 1):
        fx = {"name": name, "request": request, "expect": {"error_path": path}}
        (qdir / f"{j:02d}-{name}.json").write_text(json.dumps(fx, indent=1) + "\n")
        print(f"{j:02d} {name}: refusal at {path}")


if __name__ == "__main__":
    main()
