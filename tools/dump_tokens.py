"""Dump Python minisolr's index-time tokens for every text field in a docs file, for `minisolr-rs parity`.

Runs inside the Python minisolr project (it imports `minisolr.schema`), so use its environment:

    uv run --project ../minisolr python tools/dump_tokens.py ../minisolr/conf ../minisolr/fixtures/docs.jsonl > tokens.jsonl
    cargo run --release -- parity ../minisolr/conf tokens.jsonl

Each document is expanded with copyFields first (like `Store._effective`), so copy destinations such
as `title_suggest` are covered too. Every field whose schema kind is `text` is dumped, static or
dynamic, one JSON line per value: {"field", "value", "main", "extra"}.
"""

import json
import sys

from minisolr.schema import Schema


def effective(schema: Schema, doc: dict) -> dict:
    eff = dict(doc)
    for src, dst in schema.copy_fields:
        if src in doc:
            old = eff.get(dst, [])
            eff[dst] = (old if isinstance(old, list) else [old]) + (doc[src] if isinstance(doc[src], list) else [doc[src]])
    return eff


def main() -> None:
    schema = Schema(sys.argv[1])
    with open(sys.argv[2]) as f:
        for line in f:
            if not line.strip():
                continue
            for name, raw in effective(schema, json.loads(line)).items():
                field = schema.get(name)
                if field is None or field.kind != "text" or field.type.index_analyzer is None:
                    continue
                for v in raw if isinstance(raw, list) else [raw]:
                    if v is None:
                        continue
                    main_tokens, extra = field.type.index_analyzer.index(str(v))
                    print(json.dumps({"field": name, "value": str(v), "main": main_tokens, "extra": extra}, ensure_ascii=False))


if __name__ == "__main__":
    main()
