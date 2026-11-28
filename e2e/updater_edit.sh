#!/bin/bash
# Runs inside the fast_web container: edit a work and create a list through the app (token passed as $1).
set -e
B=http://localhost:8080
TOKEN=$1
curl -s -c /tmp/c.txt -X POST $B/account/login.json -H "Content-Type: application/json" -d '{"username":"openlibrary","password":"openlibrary"}' >/dev/null
curl -s $B/works/OL45310W.json > /tmp/w.json
TOKEN=$TOKEN python3 - <<'P'
import json, os
w = json.load(open('/tmp/w.json')); w['title'] = 'Robinson Crusoe ' + os.environ['TOKEN']; json.dump(w, open('/tmp/w2.json', 'w'))
P
curl -s -b /tmp/c.txt -X PUT $B/works/OL45310W.json -H "Content-Type: application/json" -d @/tmp/w2.json >/dev/null
curl -s -b /tmp/c.txt -X POST $B/people/openlibrary/lists.json -H "Content-Type: application/json" -d "{\"name\":\"List $TOKEN\",\"description\":\"x\",\"seeds\":[{\"key\":\"/works/OL45310W\"}]}" >/dev/null
