#!/usr/bin/env python3
"""An independent seed oracle for the fixtures.

Computes, in plain Python and without reading the Rust code, what each fixture request must
answer over fixtures/github-issues/docs.json, and writes the fixture files. It covers only the
constructs the fixtures use. Once the Rust evaluator agrees with these files, the evaluator is
the reference and this script is history; keep it for re-seeding if the corpus changes.

    python3 dev/python_oracle.py
"""
import json
import re
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent / "fixtures" / "github-issues"
DOCS = json.load(open(ROOT / "docs.json"))
MAPPING = json.load(open(ROOT / "mapping.json"))["properties"]
MS_DAY = 86_400_000


def ftype(field):
    t = MAPPING[field]["type"]
    return {"long": "number", "boolean": "boolean"}.get(t, t)


def parse_iso(s):
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    d = datetime.fromisoformat(s)
    if d.tzinfo is None:
        d = d.replace(tzinfo=timezone.utc)
    return int(d.timestamp() * 1000)


def values(doc, field):
    v = doc.get(field)
    if v is None:
        return []
    items = [x for x in v if x is not None] if isinstance(v, list) else [v]
    t = ftype(field)
    if t == "date":
        return [parse_iso(x) for x in items]
    return items


def tokens(s):
    return [t.lower() for t in re.findall(r"[^\W_]+", s)]


def norm(field, v):
    t = ftype(field)
    if t == "date":
        return parse_iso(v)
    if t in ("keyword", "text") and isinstance(v, str):
        return v.lower()
    return v


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
        want = norm(field, want)
        return any(norm(field, v) == want if ftype(field) != "date" else v == want for v in values(doc, field))
    if kind == "terms":
        (field, vals), = body.items()
        wanted = {norm(field, v) for v in vals}
        return any((norm(field, v) if ftype(field) != "date" else v) in wanted for v in values(doc, field))
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


def as_list(v):
    if v is None:
        return []
    return v if isinstance(v, list) else [v]


def es_double(n):
    return f"{int(n)}.0" if float(n).is_integer() else repr(float(n))


def num(v):
    return int(v) if float(v).is_integer() else v


def iso(ms):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%S.") + f"{d.microsecond // 1000:03d}Z"


def month_start(ms):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    return int(datetime(d.year, d.month, 1, tzinfo=timezone.utc).timestamp() * 1000)


def next_month(ms):
    d = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    y, m = (d.year + 1, 1) if d.month == 12 else (d.year, d.month + 1)
    return int(datetime(y, m, 1, tzinfo=timezone.utc).timestamp() * 1000)


def key_sort(k):
    # bool < number < string, as the Rust Key ordering does
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
                buckets.append({"key": k, "doc_count": c})
        return {"doc_count_error_upper_bound": 0, "sum_other_doc_count": sum(c for _, c in rest), "buckets": buckets}
    if kind == "range":
        out = []
        # Elasticsearch orders range buckets by from, then to, open ends first and last
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
        assert body["calendar_interval"] in ("month", "1M")
        counts = {}
        for d in docs:
            for b in {month_start(v) for v in values(d, field)}:
                counts[b] = counts.get(b, 0) + 1
        out = []
        if counts:
            b = min(counts)
            while b <= max(counts):
                c = counts.get(b, 0)
                if c >= body.get("min_doc_count", 0):
                    out.append({"key_as_string": iso(b), "key": b, "doc_count": c})
                b = next_month(b)
        return {"buckets": out}
    if kind == "value_count":
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
                    # strings: desc is emulated by sorting separately below
                    parts.append((0, v))
                else:
                    parts.append((0, -v if order == "desc" else v))
        return parts + [d["_id"]]

    for s in sorts:
        if not isinstance(s, str):
            (field, spec), = s.items()
            order = spec if isinstance(spec, str) else spec.get("order", "asc")
            assert not (order == "desc" and ftype(field) in ("keyword",)), "the oracle only sorts strings ascending"
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
    ("state_terms", {"size": 0, "aggs": {"by_state": {"terms": {"field": "state"}}}}),
    ("must_not_term_state_reason", {"size": 0, "query": {"bool": {"must_not": {"term": {"state_reason": "completed"}}}}}),
    ("must_not_term_draft", {"size": 0, "query": {"bool": {"must_not": {"term": {"draft": True}}}}}),
    ("must_not_range_closed_at", {"size": 0, "query": {"bool": {"must_not": {"range": {"closed_at": {"gte": "2026-09-01"}}}}}}),
    ("open_bugs_by_label", {
        "query": {"bool": {"filter": [{"term": {"state": "open"}}, {"terms": {"labels": ["bug"]}}]}},
        "aggs": {"labels": {"terms": {"field": "labels", "size": 5}}},
        "sort": [{"comments": "desc"}], "size": 10}),
    ("comments_gte_5_stats", {"size": 0, "query": {"range": {"comments": {"gte": 5}}},
        "aggs": {"c": {"stats": {"field": "comments"}}, "avg": {"avg": {"field": "comments"}}, "mx": {"max": {"field": "reactions"}}}}),
    ("open_created_per_month", {"size": 0, "query": {"term": {"state": "open"}},
        "aggs": {"per_month": {"date_histogram": {"field": "created_at", "calendar_interval": "month"}}}}),
    ("open_created_per_month_min1", {"size": 0, "query": {"term": {"state": "open"}},
        "aggs": {"per_month": {"date_histogram": {"field": "created_at", "calendar_interval": "month", "min_doc_count": 1}}}}),
    ("match_title_facet", {"size": 0, "query": {"match": {"title": "facet"}}}),
    ("match_title_and", {"size": 0, "query": {"match": {"title": {"query": "facet counts", "operator": "and"}}}}),
    ("match_title_or", {"size": 0, "query": {"match": {"title": {"query": "facet counts", "operator": "or"}}}}),
    ("match_phrase_title", {"size": 0, "query": {"match_phrase": {"title": "chat completions"}}}),
    ("exists_closed_by", {"size": 0, "query": {"exists": {"field": "closed_by"}}}),
    ("not_exists_closed_by", {"size": 0, "query": {"bool": {"must_not": {"exists": {"field": "closed_by"}}}}}),
    ("comments_histogram", {"size": 0, "aggs": {"h": {"histogram": {"field": "comments", "interval": 5}}}}),
    ("comments_ranges", {"size": 0, "aggs": {"r": {"range": {"field": "comments", "ranges": [{"to": 1}, {"from": 1, "to": 5}, {"from": 5, "key": "many"}]}}}}),
    ("should_open_or_completed", {"size": 0, "query": {"bool": {"should": [{"term": {"state": "open"}}, {"term": {"state_reason": "completed"}}]}}}),
    ("should_with_must_is_optional", {"size": 0, "query": {"bool": {"must": {"term": {"state": "closed"}}, "should": [{"term": {"author": "nobody-here"}}]}}}),
    ("should_with_must_msm1", {"size": 0, "query": {"bool": {"must": {"term": {"state": "closed"}}, "should": [{"term": {"state_reason": "completed"}}], "minimum_should_match": 1}}}),
    ("prefix_author", {"size": 0, "query": {"prefix": {"author": "ker"}}}),
    ("prefix_title_token", {"size": 0, "query": {"prefix": {"title": "fac"}}}),
    ("sort_comments_desc", {"query": {"match_all": {}}, "sort": [{"comments": {"order": "desc"}}], "size": 5}),
    ("sort_created_asc_from", {"query": {"term": {"state": "open"}}, "sort": ["created_at"], "size": 4, "from": 2}),
    ("term_case_insensitive", {"size": 0, "query": {"term": {"state": "OPEN"}}}),
    ("cardinality_and_value_count", {"size": 0, "aggs": {"authors": {"cardinality": {"field": "author"}}, "label_values": {"value_count": {"field": "labels"}}, "n": {"value_count": {"field": "closed_by"}}}}),
    ("source_fields", {"query": {"match_all": {}}, "size": 1, "_source": ["title", "state"]}),
    ("source_false", {"query": {"match_all": {}}, "size": 2, "_source": False}),
    ("post_filter_keeps_aggs", {"size": 0, "aggs": {"by_state": {"terms": {"field": "state"}}}, "post_filter": {"term": {"state": "open"}}}),
    ("draft_terms_boolean", {"size": 0, "aggs": {"d": {"terms": {"field": "draft"}}}}),
    ("author_terms_key_asc", {"size": 0, "aggs": {"a": {"terms": {"field": "author", "size": 3, "order": {"_key": "asc"}}}}}),
    ("author_type_count_asc", {"size": 0, "aggs": {"a": {"terms": {"field": "author_type", "order": {"_count": "asc"}}}}}),
    ("terms_date_field", {"size": 0, "query": {"term": {"created_at": "2026-10-08T15:41:59Z"}}}),
    ("range_date_window", {"size": 0, "query": {"range": {"created_at": {"gte": "2026-07-01", "lt": "2026-10-01"}}},
        "aggs": {"by_state": {"terms": {"field": "state"}}}}),
    ("page_past_the_end", {"query": {"match_all": {}}, "size": 5, "from": 334}),
    ("filter_vs_must_same", {"size": 0, "query": {"bool": {"must": [{"term": {"state": "open"}}], "filter": [{"exists": {"field": "closed_by"}}]}}}),
    # Pinned to Elasticsearch 9.5.5's answers on 2026-10-10 (dev/es_parity.py): 269, 67, 95
    ("range_date_lte_rounds_up", {"size": 0, "query": {"range": {"created_at": {"lte": "2026-09-01"}}}}),
    ("range_date_gt_rounds_up", {"size": 0, "query": {"range": {"created_at": {"gt": "2026-09-01"}}}}),
    ("msm_zero_should_only_needs_one", {"size": 0, "query": {"bool": {"should": [{"term": {"state": "open"}}], "minimum_should_match": 0}}}),
    ("range_agg_ordered_by_from", {"size": 0, "aggs": {"r": {"range": {"field": "reactions", "ranges": [{"from": 0, "to": 1}, {"from": 1}, {"to": 0}]}}}}),
    # Tokenizer divergences from Elasticsearch's standard analyzer, pinned to the profile's answer
    ("match_apostrophe_token", {"size": 0, "query": {"match": {"title": "doesn"}}}),
    ("match_dotted_token", {"size": 0, "query": {"match": {"title": "v1"}}}),
]

ERRORS = [
    ("err_wildcard", {"query": {"bool": {"filter": [{"term": {"state": "open"}}, {"wildcard": {"title": "fa*"}}]}}}, "query.bool.filter[1].wildcard"),
    ("err_unknown_field", {"query": {"term": {"label": "bug"}}}, "query.term.label"),
    ("err_quoted_number", {"query": {"range": {"comments": {"gte": "5"}}}}, "query.range.comments.gte"),
    ("err_month_not_date", {"aggs": {"x": {"terms": {"field": "state"}}}, "query": {"range": {"created_at": {"gte": "2026-07"}}}}, "query.range.created_at.gte"),
    ("err_terms_on_text", {"aggs": {"by": {"terms": {"field": "title"}}}}, "aggs.by.terms.field"),
    ("err_not_elasticsearch", {"filters": [{"field": "state", "eq": "open"}]}, "$.filters"),
    ("err_sub_aggregation", {"aggs": {"by": {"terms": {"field": "state"}, "aggs": {"c": {"avg": {"field": "comments"}}}}}}, "aggs.by.aggs"),
    ("err_sort_array", {"sort": ["labels"]}, "sort[0].labels"),
    ("err_match_on_keyword", {"query": {"match": {"state": "open"}}}, "query.match.state"),
    ("err_size_too_big", {"size": 20000}, "size"),
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
