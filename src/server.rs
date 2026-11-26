//! A blocking HTTP front end on `tiny_http`, shaped like Solr's JSON responses.

use std::time::Instant;

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::Error;
use crate::query::{Params, Selection, select};
use crate::store::Store;

/// Serves requests one at a time on `addr` until the process is stopped.
///
/// # Errors
/// Fails if the socket cannot be bound.
pub fn serve(store: &Store, addr: &str) -> Result<(), Error> {
    let server =
        Server::http(addr).map_err(|e| Error::Other(format!("cannot bind {addr}: {e}")))?;
    eprintln!(
        "listening on http://{addr}/solr/openlibrary/ ({} docs)",
        store.docs.len()
    );
    for mut request in server.incoming_requests() {
        let (status, body) = handle(store, &mut request);
        let json_type = Header::from_bytes("Content-Type", "application/json; charset=utf-8");
        let mut response = Response::from_string(body.to_string()).with_status_code(status);
        if let Ok(h) = json_type {
            response = response.with_header(h);
        }
        if let Err(e) = request.respond(response) {
            eprintln!("warning: could not send response: {e}");
        }
    }
    Ok(())
}

fn handle(store: &Store, request: &mut Request) -> (u16, Value) {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let mut pairs = parse_form(query);
    if *request.method() == Method::Post {
        let mut body = String::new();
        if let Err(e) = request.as_reader().read_to_string(&mut body) {
            eprintln!("warning: unreadable request body: {e}");
        }
        pairs.extend(parse_form(&body));
    }
    route(store, path, &Params::new(pairs))
}

/// Routes a request path to its handler; returns the HTTP status and the JSON body.
#[must_use]
pub fn route(store: &Store, path: &str, params: &Params) -> (u16, Value) {
    match path.trim_end_matches('/') {
        "/solr/openlibrary/admin/ping" => (
            200,
            json!({"responseHeader": {"status": 0, "QTime": 0}, "status": "OK"}),
        ),
        "/solr/openlibrary/select" => (200, select_response(store, params)),
        other => {
            eprintln!("warning: unsupported path {other}");
            (
                404,
                json!({"error": {"code": 404, "msg": format!("not found: {other}")}}),
            )
        }
    }
}

/// A Solr-shaped `/select` response; unsupported requests become an empty result.
#[must_use]
pub fn select_response(store: &Store, params: &Params) -> Value {
    let started = Instant::now();
    let mut warning = None;
    let selection = select(store, params).unwrap_or_else(|e| {
        eprintln!(
            "warning: UNSUPPORTED (ignored): {e}; q={:?}",
            params.get("q").unwrap_or_default()
        );
        warning = Some(e.to_string());
        Selection {
            num_found: 0,
            start: 0,
            docs: Vec::new(),
        }
    });
    let mut header =
        json!({"status": 0, "QTime": started.elapsed().as_millis(), "params": params.to_json()});
    if let (Some(w), Some(map)) = (warning, header.as_object_mut()) {
        map.insert("warning".into(), Value::String(w));
    }
    json!({
        "responseHeader": header,
        "response": {"numFound": selection.num_found, "start": selection.start, "numFoundExact": true, "docs": selection.docs},
    })
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
