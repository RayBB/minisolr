#!/usr/bin/env python3
"""Write a synthetic data set of N rows by cloning fixtures/docs.jsonl with new keys (standard library only).

    python3 tools/gen_docs.py 10000 /tmp/ms10k/docs.jsonl

Each round copies every fixture row with `-<round>` appended to its `key` and `_root_`, and a round
number added to the title so clones are not identical. The output is not committed.
"""

import json
import os
import sys


def main() -> None:
    n, out = int(sys.argv[1]), sys.argv[2]
    src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "fixtures", "docs.jsonl")
    rows = [json.loads(line) for line in open(src) if line.strip()]
    os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
    written = 0
    with open(out, "w") as f:
        r = 0
        while written < n:
            for row in rows:
                if written >= n:
                    break
                row = dict(row)
                row["key"] = f"{row['key']}-{r}"
                if "_root_" in row:
                    row["_root_"] = f"{row['_root_']}-{r}"
                if isinstance(row.get("title"), str):
                    row["title"] = f"{row['title']} vol {r}"
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
                written += 1
            r += 1


if __name__ == "__main__":
    main()
