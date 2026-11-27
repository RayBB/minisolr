"""Dump Python minisolr's index-time and query-time tokens over a broad synthetic set, for `minisolr-rs parity`.

Runs inside the Python minisolr project (it imports `minisolr.schema`), so use its environment:

    uv run --project ../minisolr python tools/synthetic_tokens.py ../minisolr/conf ../minisolr/fixtures/docs.jsonl > synthetic.jsonl
    cargo run --release -- parity ../minisolr/conf synthetic.jsonl

An optional third argument names a file of extra inputs, one per line (for example /usr/share/dict/words, to
exercise the stemmers on a large vocabulary).

For every text field type that a field or dynamic field of the schema uses, the inputs are every distinct
whitespace-separated token of every string value in the docs file, every distinct whole value, and a few
hundred crafted strings (synonyms, possessives, accents, hyphenation, digit/letter mixes, case changes,
protected and stop words, CJK, Cyrillic, Arabic, Indic and other scripts, odd whitespace and symbols).
One JSON line per (type, input): {"type", "value", "main", "extra", "query"}, where `query` is the list of
positions with their alternatives.
"""

import itertools
import json
import sys
from pathlib import Path

from minisolr.schema import Schema

CRAFTED = [
    # synonyms (conf/synonyms.txt) in several cases, alone and in context
    "aaafoo", "AAAFOO", "bbbfoo", "cccfoo", "fooaaa", "baraaa", "BAZAAA", "GB", "gib", "gigabyte", "Gigabytes",
    "MB mib", "megabyte", "Television", "televisions", "TV", "TVs", "pixima", "a TV set", "8 GB card", "tv-show",
    "TV's", "gb/mb", "Pixima Pixma",
    # possessives and apostrophes
    "Gulliver's Travels", "O'Brien's", "O’Brien’s", "James'", "boss's", "BOSS'S", "'tis", "rock'n'roll",
    "o'clock", "''", "'", "it's", "Children's books", "dog’s life", "s's", "'s", "x's'",
    # accents, ligatures, special letters
    "Café Müller", "Ångström", "Straße", "STRASSE", "ŒUVRES complètes", "Łódź", "Þór", "İstanbul", "ıi", "ΣΟΦΙΑ ΟΔΟΣ",
    "σοφίας", "Ærø", "Ðorđe", "ħ", "ﬁnancial ﬂow", "ＡＢＣ　ｄｅｆ", "𝐀𝐁𝐂", "ẞ", "ǅemal", "Ǆ", "ǈ", "µ", "ſ", "Ꭰꭰ",
    "été", "ä", "ñ", "İ", "Dvořák", "Brontë", "Gödel, Escher, Bach", "Nguyễn",
    # hyphens and delimiters
    "Wi-Fi", "e-mail", "x-ray-vision", "--", "-a-", "a--b", "self-help", "peace--one", "20th-century", "A-1",
    "U.S.A.", "e.g.", "a.b.c", "a/b", "a_b", "snake_case", "AT&T", "a&b", "C#", "C++", "#hashtag", "©2020", "(c)",
    "[v.1.]", "v.1", "1/2", "foo@bar.com", "http://example.com/a-b", "~tilde", "a|b", "under_score_",
    # digits and letters
    "html5", "HTML5", "MP3", "3D", "B2B", "802.11b", "1st", "2nd", "21st", "1,000", "3.14", "A1B2C3", "²³", "x²",
    "Ⅻ", "ⅻ", "½", "١٢٣", "٣ كتب", "१२३", "๑๒๓", "0x1F", "1984", "1e10", "ISBN 978-0-14-044913-6", "9780140449136",
    "OL123W", "OL45310W", "v1.2.3", "2001: A Space Odyssey", "Catch-22", "R2-D2", "4x4", "100%",
    # case changes
    "PowerShot", "iPhone", "McDonald", "camelCaseWord", "ABCdef", "ABC", "aBC", "XMLHttpRequest", "eBay", "LaTeX",
    "MacBook Pro", "DeVito", "o'NEIL", "TOM SAWYER", "tom sawyer",
    # protected and stop words
    "dontstems", "DONTSTEMS", "zwhacky", "Zwhacky", "the", "The Lord of the Rings", "a an the of and", "running jumps",
    "Odyssey", "generalizations", "abilities", "ponies", "caresses", "series", "glasses", "toes", "bus", "is",
    "species", "news", "cats", "theses", "flies", "hopping", "happiness", "relational", "conditional",
    # scripts
    "日本語の本", "中文书", "한국어 텍스트", "カタカナ", "ひらがな", "漢字かな交じり文", "Война и мир", "Достоевский Ф.М.",
    "ЁЛКА ёлка", "ألف ليلة وليلة", "كتاب", "الكتاب", "שלום עולם", "हिन्दी साहित्य", "తెలుగు", "తెలుగు సాహిత్యం",
    "ภาษาไทย", "ქართული", "Ελληνικά", "Հայերեն", "தமிழ்", "ಕನ್ನಡ", "മലയാളം", "বাংলা", "ગુજરાતી", "ਪੰਜਾਬੀ", "සිංහල",
    "ꯃꯤꯇꯩ", "𐌰𐌱", "Tiếng Việt", "Türkçe", "Polski język", "Română", "Íslenska",
    # whitespace and odd characters
    "", " ", "  two  spaces  ", "tab\tsep", "new\nline", "a\x1cb", "a\x1fb", "nbsp here", "zero​width",
    "joiner‍here", "em space", "ideographic　space", "﻿bom", "📚 books", "I ❤ NY", "a\u0000b",
    "dash–en dash—em", "quote “double” ‘single’", "ellipsis…", "«guillemets»",
    "­ soft­hyphen", "ﬀ", "™", "℃", "№5", "Ⓐ", "ⓐ", "🄰", "ª º", "ʰ",
]


def text_types(schema: Schema) -> dict[str, object]:
    """The text field types used by a field or dynamic field, by name."""
    out = {}
    for f in [*schema.fields.values(), *schema.dynamic]:
        if f.kind == "text":
            out[f.type.name] = f.type
    return dict(sorted(out.items()))


def inputs(docs_path: Path) -> list[str]:
    seen: dict[str, None] = {}

    def walk(v: object) -> None:
        if isinstance(v, str):
            seen[v] = None
            for t in v.split():
                seen[t] = None
        elif isinstance(v, list):
            for x in v:
                walk(x)

    for line in docs_path.read_text(encoding="utf-8").splitlines():
        if line.strip():
            for v in json.loads(line).values():
                walk(v)
    for s in CRAFTED:
        seen[s] = None
    # Pairs of crafted strings joined by a space and by a hyphen, for interactions.
    for a, b in itertools.islice(itertools.combinations(CRAFTED[:80], 2), 300):
        seen[f"{a} {b}"] = None
        seen[f"{a}-{b}"] = None
    return list(seen)


def main() -> None:
    schema = Schema(sys.argv[1])
    values = inputs(Path(sys.argv[2]))
    if len(sys.argv) > 3:
        values += [w for w in Path(sys.argv[3]).read_text(encoding="utf-8").splitlines() if w]
    for name, t in text_types(schema).items():
        for v in values:
            if t.index_analyzer is not None:
                main_tokens, extra = t.index_analyzer.index(v)
            else:
                main_tokens, extra = [v], []
            query = t.query_analyzer.query(v) if t.query_analyzer is not None else [[v]]
            print(json.dumps({"type": name, "value": v, "main": main_tokens, "extra": extra, "query": query}, ensure_ascii=False))


if __name__ == "__main__":
    main()
