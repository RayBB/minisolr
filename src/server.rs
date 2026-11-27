//! The HTTP front end on `tiny_http`, shaped like Solr's JSON responses, and [`Core`],
//! the shared state behind it.
//!
//! A few worker threads take requests from one listener. The store sits behind an
//! `RwLock`; updates hold the write lock for a whole request (as Python holds its store
//! lock) and save the store before releasing it. Searches never hold the store lock
//! while they run: they take an `Arc` of the current index snapshot, rebuilt lazily
//! (by one thread at a time, outside the store lock) when the store version has moved
//! on. Lock order is store, then cache; the cache lock is only held to swap the `Arc`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use serde_json::{Map, Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::Error;
use crate::index::Index;
use crate::query::{self, Params, Selection};
use crate::schema::Schema;
use crate::store::Store;
use crate::update;

/// The core name in every path.
pub const CORE: &str = "openlibrary";

/// Worker threads serving requests.
const WORKERS: usize = 4;

/// The store and its derived index, shared by the worker threads.
pub struct Core {
    schema: Arc<Schema>,
    store: RwLock<Store>,
    /// The store version, readable without the store lock.
    version: AtomicU64,
    /// The newest index snapshot built.
    cache: Mutex<Option<Arc<Index>>>,
    /// Held while a search rebuilds the index, so only one thread builds it.
    rebuild: Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Core {
    /// Wraps a store; the index is built by the first search.
    #[must_use]
    pub fn new(store: Store, schema: Arc<Schema>) -> Self {
        let version = store.version;
        Self {
            schema,
            store: RwLock::new(store),
            version: AtomicU64::new(version),
            cache: Mutex::new(None),
            rebuild: Mutex::new(()),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Store> {
        self.store.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Store> {
        self.store.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of rows in the store.
    #[must_use]
    pub fn num_docs(&self) -> usize {
        self.read().len()
    }

    fn cached(&self, version: u64) -> Option<Arc<Index>> {
        lock(&self.cache)
            .as_ref()
            .filter(|idx| idx.version >= version)
            .map(Arc::clone)
    }

    fn publish(&self, index: &Arc<Index>) {
        let mut cache = lock(&self.cache);
        if cache.as_ref().is_none_or(|c| c.version < index.version) {
            *cache = Some(Arc::clone(index));
        }
    }

    /// The index for the current store version, rebuilt if the store has changed.
    ///
    /// # Errors
    /// Fails if Tantivy cannot build the index.
    pub fn snapshot(&self) -> Result<Arc<Index>, Error> {
        if let Some(idx) = self.cached(self.version.load(Ordering::Acquire)) {
            return Ok(idx);
        }
        let _building = lock(&self.rebuild);
        if let Some(idx) = self.cached(self.version.load(Ordering::Acquire)) {
            return Ok(idx);
        }
        let (version, rows) = {
            let store = self.read();
            (store.version, store.rows())
        };
        let index = Arc::new(Index::from_docs(Arc::clone(&self.schema), rows, version)?);
        self.publish(&index);
        Ok(index)
    }

    /// The index for `store` as it is now, for delete-by-query inside an update (the
    /// caller holds the store write lock).
    fn index_for(&self, store: &Store) -> Result<Arc<Index>, Error> {
        if let Some(idx) = self.cached(store.version) {
            return Ok(idx);
        }
        let index = Arc::new(Index::from_docs(
            Arc::clone(&self.schema),
            store.rows(),
            store.version,
        )?);
        self.publish(&index);
        Ok(index)
    }

    /// Applies an update body; returns the HTTP status and the JSON response.
    pub fn update(&self, body: &str, params: &Params) -> (u16, Value) {
        let mut store = self.write();
        let uk = self.schema.unique_key.clone();
        let mut delete_query = |s: &Store, q: &str| -> Result<Vec<String>, Error> {
            let index = self.index_for(s)?;
            Ok(query::matching_rows(&index, q)?
                .into_iter()
                .filter_map(|ord| {
                    index
                        .row(ord)
                        .get(&uk)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect())
        };
        let outcome = update::apply(&mut store, body, params, &mut delete_query);
        self.version.store(store.version, Ordering::Release);
        if outcome.save
            && let Err(e) = store.save()
        {
            eprintln!("error: could not save the store: {e}");
            return (
                400,
                json!({"responseHeader": {"status": 400}, "error": {"msg": format!("OSError: {e}"), "code": 400}}),
            );
        }
        (outcome.status, outcome.body)
    }
}

/// Serves requests on `addr` with a few worker threads until the process is stopped.
///
/// # Errors
/// Fails if the socket cannot be bound.
pub fn serve(core: &Arc<Core>, addr: &str) -> Result<(), Error> {
    let server =
        Arc::new(Server::http(addr).map_err(|e| Error::Other(format!("cannot bind {addr}: {e}")))?);
    let note = if core.read().persistent() {
        ""
    } else {
        ", updates are not saved"
    };
    eprintln!(
        "listening on http://{addr}/solr/{CORE}/ ({} docs{note})",
        core.num_docs()
    );
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let server = Arc::clone(&server);
            let core = Arc::clone(core);
            std::thread::spawn(move || {
                while let Ok(request) = server.recv() {
                    respond(&core, request);
                }
            })
        })
        .collect();
    for w in workers {
        if w.join().is_err() {
            eprintln!("warning: a worker thread panicked");
        }
    }
    Ok(())
}

fn respond(core: &Core, mut request: Request) {
    let (status, body) = handle(core, &mut request);
    let json_type = Header::from_bytes("Content-Type", "application/json; charset=utf-8");
    let mut response = Response::from_string(body.to_string()).with_status_code(status);
    if let Ok(h) = json_type {
        response = response.with_header(h);
    }
    if let Err(e) = request.respond(response) {
        eprintln!("warning: could not send response: {e}");
    }
}

fn handle(core: &Core, request: &mut Request) -> (u16, Value) {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let post = *request.method() == Method::Post;
    let mut body = String::new();
    if post && let Err(e) = request.as_reader().read_to_string(&mut body) {
        eprintln!("warning: unreadable request body: {e}");
    }
    let json_body = request
        .headers()
        .iter()
        .any(|h| h.field.equiv("Content-Type") && h.value.as_str().contains("json"));
    let request = HttpRequest {
        path,
        query: parse_form(query),
        post,
        body: &body,
        json_body,
    };
    route(core, &request)
}

/// A request as the router sees it.
#[derive(Debug)]
pub struct HttpRequest<'a> {
    /// The path without the query string.
    pub path: &'a str,
    /// The decoded query string.
    pub query: Vec<(String, String)>,
    /// Whether the method is POST.
    pub post: bool,
    /// The request body (empty for GET).
    pub body: &'a str,
    /// Whether the body is JSON (by `Content-Type`).
    pub json_body: bool,
}

impl HttpRequest<'_> {
    /// Query string parameters plus a form-encoded body.
    fn form_params(&self) -> Params {
        let mut pairs = self.query.clone();
        if self.post && !self.json_body {
            pairs.extend(parse_form(self.body));
        }
        Params::new(pairs)
    }
}

/// Routes a request to its handler; returns the HTTP status and the JSON body.
#[must_use]
pub fn route(core: &Core, request: &HttpRequest<'_>) -> (u16, Value) {
    let path = request.path.trim_end_matches('/');
    let Some(handler) = path.strip_prefix("/solr/") else {
        return not_found(path);
    };
    if handler == "admin/cores" {
        return (
            200,
            json!({"responseHeader": {"status": 0}, "status": {CORE: {"name": CORE, "index": {"numDocs": core.num_docs()}}}}),
        );
    }
    let Some(handler) = handler.strip_prefix(CORE).and_then(|h| h.strip_prefix('/')) else {
        return not_found(path);
    };
    match handler {
        "admin/ping" => (
            200,
            json!({"responseHeader": {"status": 0, "QTime": 0}, "status": "OK"}),
        ),
        "select" | "query" if request.post && request.json_body => {
            let parsed = serde_json::from_str::<Value>(request.body)
                .map_err(Error::from)
                .and_then(|body| Ok((query::json_request_params(&request.query, &body)?, body)));
            match parsed {
                Ok((params, body)) => {
                    let facet = body.get("facet").filter(|f| !f.is_null());
                    with_index(core, |idx| {
                        select_response(&params, |p| query::select_json(idx, p, facet))
                    })
                }
                Err(e) => {
                    eprintln!("error: JSON request: {e}");
                    (
                        500,
                        json!({"responseHeader": {"status": 500}, "error": {"msg": e.to_string(), "code": 500}}),
                    )
                }
            }
        }
        "select" | "query" => {
            let params = request.form_params();
            with_index(core, |idx| {
                select_response(&params, |p| query::select(idx, p))
            })
        }
        "export" => {
            let params = request.form_params();
            with_index(core, |idx| {
                select_response(&params, |p| query::export(idx, p))
            })
        }
        "get" => {
            let params = request.form_params();
            with_index(core, |idx| get(idx, &params))
        }
        "mlt" => {
            eprintln!("warning: UNSUPPORTED (ignored): /mlt returns no results");
            (
                200,
                json!({"responseHeader": {"status": 0, "QTime": 0}, "response": {"numFound": 0, "start": 0, "docs": []}}),
            )
        }
        "update" | "update/json" | "update/json/docs" if request.post => {
            core.update(request.body, &Params::new(request.query.clone()))
        }
        "update" | "update/json" | "update/json/docs" => (
            405,
            json!({"responseHeader": {"status": 405}, "error": {"msg": "Method Not Allowed", "code": 405}}),
        ),
        _ => not_found(path),
    }
}

fn not_found(path: &str) -> (u16, Value) {
    eprintln!("warning: unsupported path {path}");
    (
        404,
        json!({"error": {"code": 404, "msg": format!("not found: {path}")}}),
    )
}

fn with_index(core: &Core, f: impl FnOnce(&Index) -> (u16, Value)) -> (u16, Value) {
    match core.snapshot() {
        Ok(idx) => f(&idx),
        Err(e) => {
            eprintln!("error: index build failed: {e}");
            (
                500,
                json!({"responseHeader": {"status": 500}, "error": {"msg": e.to_string(), "code": 500}}),
            )
        }
    }
}

/// A Solr-shaped `/select` (or `/export`) response from `run`; an unsupported request
/// becomes an empty result with a `warning` in the header. `nextCursorMark`,
/// `facet_counts` and `facets` are added when the selection has them.
pub fn select_response(
    params: &Params,
    run: impl FnOnce(&Params) -> Result<Selection, Error>,
) -> (u16, Value) {
    let started = Instant::now();
    let mut warning = None;
    let selection = run(params).unwrap_or_else(|e| {
        eprintln!(
            "warning: UNSUPPORTED (ignored): {e}; q={:?}",
            params.get("q").unwrap_or_default()
        );
        warning = Some(e.to_string());
        Selection::empty()
    });
    let mut header =
        json!({"status": 0, "QTime": started.elapsed().as_millis(), "params": params.to_json()});
    if let (Some(w), Some(map)) = (warning, header.as_object_mut()) {
        map.insert("warning".into(), Value::String(w));
    }
    let mut out = Map::new();
    out.insert("responseHeader".into(), header);
    out.insert(
        "response".into(),
        json!({"numFound": selection.num_found, "start": selection.start, "numFoundExact": true, "docs": selection.docs}),
    );
    if let Some(c) = selection.next_cursor {
        out.insert("nextCursorMark".into(), Value::String(c));
    }
    if let Some(fc) = selection.facet_counts {
        out.insert("facet_counts".into(), fc);
    }
    if let Some(f) = selection.facets {
        out.insert("facets".into(), f);
    }
    if let Some(h) = selection.highlighting {
        out.insert("highlighting".into(), h);
    }
    if let Some(sc) = selection.spellcheck {
        out.insert("spellcheck".into(), sc);
    }
    (200, Value::Object(out))
}

/// `/get`, as Python's `get`: `id` (repeatable) and `ids` (comma-separated); `fl` is
/// split on commas. One id answers `{"doc": ...}` (`null` when missing); `ids` or
/// several ids answer a `response` with the documents found, in request order.
#[must_use]
pub fn get(index: &Index, params: &Params) -> (u16, Value) {
    let ids: Vec<&str> = params
        .all("id")
        .chain(params.all("ids").flat_map(|v| v.split(',')))
        .collect();
    let fl = query::FieldList::parse(params.get("fl").unwrap_or("*"), &[',']);
    let one = |key: &str| -> Option<Value> {
        let ord = index.ord_of_key(key)?;
        match query::project(index, ord, 0.0, &fl, params) {
            Ok(doc) => Some(doc),
            Err(e) => {
                eprintln!("warning: /get {key}: {e}");
                None
            }
        }
    };
    if params.get("ids").is_some() || ids.len() > 1 {
        let docs: Vec<Value> = ids.iter().filter_map(|k| one(k)).collect();
        return (
            200,
            json!({"response": {"numFound": docs.len(), "start": 0, "docs": docs}}),
        );
    }
    (200, json!({"doc": ids.first().and_then(|k| one(k))}))
}

/// Decodes an `application/x-www-form-urlencoded` string.
#[must_use]
pub fn parse_form(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = bytes.next().and_then(|c| char::from(c).to_digit(16));
                let lo = bytes.next().and_then(|c| char::from(c).to_digit(16));
                if let (Some(hi), Some(lo)) = (hi, lo)
                    && let Ok(byte) = u8::try_from(hi * 16 + lo)
                {
                    out.push(byte);
                }
            }
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_forms() {
        assert_eq!(
            parse_form("q=a+b%26c&fq=type%3Awork&x"),
            [
                ("q".into(), "a b&c".into()),
                ("fq".into(), "type:work".into()),
                ("x".into(), String::new())
            ]
        );
        assert_eq!(percent_decode("%E2%80%99"), "\u{2019}");
    }
}
