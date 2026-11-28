#!/bin/bash
# Runs inside fast_web: create a list, wait for the solr-updater to index it, delete it, expect it gone from list search.
B=http://localhost:8080
C="curl -s --max-time 20"
TOKEN=$1
$C -c /tmp/c.txt -X POST $B/account/login.json -H "Content-Type: application/json" -d '{"username":"openlibrary","password":"openlibrary"}' >/dev/null
KEY=$($C -b /tmp/c.txt -X POST $B/people/openlibrary/lists.json -H "Content-Type: application/json" -d "{\"name\":\"Deletable $TOKEN\",\"description\":\"x\",\"seeds\":[{\"key\":\"/works/OL45310W\"},{\"key\":\"/works/OL20600W\"}]}" | python3 -c "import sys,json;print(json.load(sys.stdin)['key'])")
count() { $C "$B/search/lists.json?q=$TOKEN" | python3 -c "import sys,json;print(len(json.load(sys.stdin)['docs']))" 2>/dev/null || echo ERR; }
for i in $(seq 1 30); do [ "$(count)" = 1 ] && break; sleep 2; done
echo "created $KEY; searchable after create: $(count)"
$C -b /tmp/c.txt -X POST "$B$KEY/delete.json" >/dev/null
for i in $(seq 1 30); do [ "$(count)" = 0 ] && break; sleep 2; done
echo "after delete: $(count)"
