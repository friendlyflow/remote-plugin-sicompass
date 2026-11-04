//! Remote, a sicompass plugin: browse FFON served over HTTP.
//!
//! A server answers `GET <url>/root` with a JSON FFON array, and `GET
//! <url>/<entry>` with the children of a top-level entry. Remote shows each
//! server the user configured as a section of its own, fetches a level when it
//! is opened, and sends the server's API key as a bearer token.
//!
//! The servers are the user's, so the plugin declares that it reaches any
//! server (`"allowedHosts": ["*"]`), which the Store shows before install and
//! the user approves. It connects to the servers the user configured and to
//! nothing else.
//!
//! Configured in its settings section, one server per line:
//!
//! ```text
//! servers:  products https://ffon.example/api
//!           wiki https://wiki.example
//! API keys: products the-token
//! ```
//!
//! It was a built-in of the sicompass app (`lib/lib_remote`, a provider per
//! server, before that a TypeScript script). [`Remote`] is the tree logic,
//! tested with `cargo test`; [`RemotePlugin`] connects it to the plugin
//! interface and fetches with its own HTTP client, and `src/main.rs` makes it
//! the program.
//!
//! `plugin.json` declares `markup`, so what another machine sends is escaped on
//! its way into a row (see [`escape_row`]): a server can make a row show `<b>`, but
//! it cannot make it bold, or make it a button. A server's links and images are
//! the exception, and work as they always did.

use std::collections::HashMap;
use std::time::Duration;

use sicompass_sdk::ffon::{FfonElement, parse_json_value};
use sicompass_sdk::plugin::{Descriptor, Plugin, host};
use sicompass_sdk::tags;

/// How long one request may take, all of it. A fetch runs on a call from the
/// app, which waits for it, so a server that hangs becomes an error row
/// instead of an app that waits forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `GET url` with an optional bearer key: `(status, body)`, or why not.
pub type Get = Box<dyn Fn(&str, &str) -> Result<(u16, Vec<u8>), String>>;

/// One configured server.
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub name: String,
    pub url: String,
    pub key: String,
}

/// Parse the two settings: `name URL` and `name key`, one per line. Blank
/// lines and lines starting with `#` are skipped, and so is a line without a
/// URL. A key for a name with no server is ignored.
pub fn parse_servers(servers: &str, keys: &str) -> Vec<Server> {
    let pairs = |text: &str| -> Vec<(String, String)> {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(|l| {
                let (name, rest) = l.split_once(char::is_whitespace)?;
                let rest = rest.trim();
                (!rest.is_empty()).then(|| (name.to_owned(), rest.to_owned()))
            })
            .collect()
    };
    let keys: HashMap<String, String> = pairs(keys).into_iter().collect();
    pairs(servers)
        .into_iter()
        .map(|(name, url)| Server {
            key: keys.get(&name).cloned().unwrap_or_default(),
            url: url.trim_end_matches('/').to_owned(),
            name,
        })
        .collect()
}

/// The tree: the servers, their root lists, and their entries' pages.
pub struct Remote {
    /// The levels open, each the text of a row. A row can hold a `/` of its own
    /// (`</b>`), so they are kept apart.
    segments: Vec<String>,
    /// `segments` joined, for `current_path`.
    path: String,
    servers: Vec<Server>,
    /// Pages by URL, kept until the settings change or `refresh`.
    cache: HashMap<String, Vec<FfonElement>>,
    get: Get,
    /// Shown at the root when no server is configured (translated by the host).
    pub no_servers_text: String,
}

impl Remote {
    pub fn new(get: Get) -> Self {
        Remote {
            segments: Vec::new(),
            path: "/".to_owned(),
            servers: Vec::new(),
            cache: HashMap::new(),
            get,
            no_servers_text: "No servers yet. Add one in Settings, under remote, as a line: \
                              name URL"
                .to_owned(),
        }
    }

    /// Take the current settings. A change forgets every fetched page.
    pub fn set_config(&mut self, servers: &str, keys: &str) {
        let servers = parse_servers(servers, keys);
        if servers != self.servers {
            self.servers = servers;
            self.cache.clear();
        }
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// Forget every fetched page.
    pub fn refresh(&mut self) {
        self.cache.clear();
    }

    pub fn current_path(&self) -> &str {
        &self.path
    }

    /// A saved path says only where its `/`s are, so a tab left inside a row with
    /// a `/` of its own comes back empty.
    pub fn set_current_path(&mut self, path: &str) {
        self.segments = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        self.sync_path();
    }

    pub fn push_path(&mut self, segment: &str) {
        if !segment.is_empty() {
            self.segments.push(segment.to_owned());
        }
        self.sync_path();
    }

    pub fn pop_path(&mut self) {
        self.segments.pop();
        self.sync_path();
    }

    fn sync_path(&mut self) {
        self.path = format!("/{}", self.segments.join("/"));
    }

    pub fn fetch(&mut self) -> Vec<FfonElement> {
        let parts = self.segments.clone();
        let Some((name, rest)) = parts.split_first() else {
            if self.servers.is_empty() {
                return vec![FfonElement::new_str(self.no_servers_text.clone())];
            }
            return self
                .servers
                .iter()
                .map(|s| FfonElement::new_obj(tags::escape(&s.name)))
                .collect();
        };
        let Some(server) = self.servers.iter().find(|s| &s.name == name).cloned() else {
            return Vec::new();
        };
        let result = match rest.split_first() {
            // The server's own list: each top-level entry opens lazily.
            None => self.page(&server, "root").map(|items| {
                items
                    .into_iter()
                    .map(|e| match e {
                        FfonElement::Obj(o) if !tags::has_link(&o.key) => {
                            FfonElement::new_obj(o.key)
                        }
                        other => other,
                    })
                    .collect()
            }),
            // An entry's page, then down its children by key.
            Some((entry, deeper)) => self.page(&server, &url_encode(entry)).map(|mut level| {
                for segment in deeper {
                    let next = level.into_iter().find_map(|e| match e {
                        FfonElement::Obj(o) if tags::strip_display(&o.key) == segment.as_str() => {
                            Some(o.children)
                        }
                        _ => None,
                    });
                    level = next.unwrap_or_default();
                }
                level
            }),
        };
        result.unwrap_or_else(|e| vec![FfonElement::new_str(tags::escape(&e))])
    }

    /// `GET <server>/<rel>`, parsed, from the cache when it is there. Errors
    /// are not cached, so the next look tries again.
    fn page(&mut self, server: &Server, rel: &str) -> Result<Vec<FfonElement>, String> {
        let url = format!("{}/{rel}", server.url);
        if let Some(cached) = self.cache.get(&url) {
            return Ok(cached.clone());
        }
        let (status, body) = (self.get)(&url, &server.key)
            .map_err(|e| format!("Error connecting to {}: {e}", server.url))?;
        if !(200..300).contains(&status) {
            return Err(format!("Failed to fetch from {}: {status}", server.url));
        }
        let items = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(serde_json::Value::Array(items)) => items,
            _ => return Err(format!("Invalid response from {}", server.url)),
        };
        let elements: Vec<FfonElement> = items
            .iter()
            .map(|item| escape_element(parse_json_value(item)))
            .collect();
        self.cache.insert(url, elements.clone());
        Ok(elements)
    }
}

/// `element` and everything in it, with its text escaped ([`escape_row`]).
fn escape_element(element: FfonElement) -> FfonElement {
    match element {
        FfonElement::Str(s) => FfonElement::Str(escape_row(&s)),
        FfonElement::Obj(mut o) => {
            o.key = escape_row(&o.key);
            o.children = o.children.into_iter().map(escape_element).collect();
            FfonElement::Obj(o)
        }
    }
}

/// A row from a server, safe to show. Every tag in it becomes text, except a
/// `<link>` or an `<image>` with its closing tag, whose insides are still escaped.
/// Those two only show something and go somewhere, and they are what a server's
/// list is for. Everything else would act on this computer: a `<button>` or an
/// `<input>` here answers to nobody.
///
/// A server writes FFON, so a `\<` it sent already means a `<`, and stays one.
fn escape_row(row: &str) -> String {
    let text = |s: &str| tags::escape(&unescape(s));
    let mut out = String::with_capacity(row.len());
    let mut rest = row;
    while let Some((at, open, close)) = next_target(rest) {
        let inside = at + open.len();
        let Some(len) = rest[inside..].find(close) else {
            break;
        };
        out.push_str(&text(&rest[..at]));
        out.push_str(open);
        out.push_str(&text(&rest[inside..inside + len]));
        out.push_str(close);
        rest = &rest[inside + len + close.len()..];
    }
    out.push_str(&text(rest));
    out
}

/// The first `<link>` or `<image>` in `text` that is not escaped: where it is, and
/// its two tags.
fn next_target(text: &str) -> Option<(usize, &'static str, &'static str)> {
    [("<link>", "</link>"), ("<image>", "</image>")]
        .into_iter()
        .filter_map(|(open, close)| {
            text.match_indices(open)
                .find(|(at, _)| !text[..*at].ends_with('\\'))
                .map(|(at, _)| (at, open, close))
        })
        .min_by_key(|(at, _, _)| *at)
}

/// Undo `tags::escape`. Safe on text that was never escaped.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('<' | '>')) {
            continue;
        }
        out.push(c);
    }
    out
}

/// Percent-encoding for one path segment: RFC 3986 unreserved characters and
/// `!*'()` stay, everything else is encoded (as `encodeURIComponent`).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

pub struct RemotePlugin {
    remote: Remote,
}

impl RemotePlugin {
    /// Read the servers from this plugin's settings section.
    fn load_config(&mut self) {
        let servers = host::get_setting("servers").unwrap_or_default();
        let keys = host::get_setting("apiKeys").unwrap_or_default();
        self.remote.set_config(&servers, &keys);
    }
}

/// The HTTP client every request goes through. An error status is an answer
/// (the tree reports it with its code), not a failure to connect.
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

/// `GET url` with `Accept: application/json`, and the key as a bearer token
/// when there is one.
fn http_get(agent: &ureq::Agent, url: &str, key: &str) -> Result<(u16, Vec<u8>), String> {
    let mut request = agent.get(url).header("Accept", "application/json");
    if !key.is_empty() {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let mut response = request.call().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok((status, body))
}

impl Plugin for RemotePlugin {
    fn new() -> Self {
        let agent = http_agent();
        RemotePlugin {
            remote: Remote::new(Box::new(move |url, key| http_get(&agent, url, key))),
        }
    }

    fn init(&mut self) {
        self.remote.no_servers_text = host::translate("remote-no-servers");
        self.load_config();
    }

    /// Each level is a blocking GET to the server, and the app waits on every
    /// call: one unreachable server would hold scroll mode until it times out.
    fn allows_scroll_prefetch(&self) -> bool {
        false
    }

    fn describe(&self) -> Descriptor {
        Descriptor {
            name: "remote".to_owned(),
            display_name: host::translate("remote-display-name"),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            ..Default::default()
        }
    }

    fn fetch(&mut self) -> Vec<FfonElement> {
        self.remote.fetch()
    }

    fn current_path(&self) -> &str {
        self.remote.current_path()
    }

    fn set_current_path(&mut self, path: &str) {
        self.remote.set_current_path(path);
    }

    fn push_path(&mut self, segment: &str) {
        self.remote.push_path(segment);
    }

    fn pop_path(&mut self) {
        self.remote.pop_path();
    }

    fn on_setting_change(&mut self, key: &str, _value: &str) {
        if key == "servers" || key == "apiKeys" {
            self.load_config();
        }
    }

    /// F5 (and the `refresh` command): fetch again.
    fn commands(&self) -> Vec<String> {
        vec!["refresh".to_owned()]
    }

    fn handle_command(
        &mut self,
        cmd: &str,
        _elem_key: &str,
        _elem_type: i32,
    ) -> Result<Option<FfonElement>, String> {
        if cmd == "refresh" {
            self.load_config();
            self.remote.refresh();
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Every level is a GET the app would wait on, so scroll mode must not
    /// fetch ahead.
    #[test]
    fn scroll_mode_may_not_fetch_ahead() {
        assert!(!<RemotePlugin as Plugin>::allows_scroll_prefetch(
            &RemotePlugin::new()
        ));
    }

    type Log = Rc<RefCell<Vec<(String, String)>>>;

    /// A fake server: `routes` maps a URL to `(status, body)`; every request is
    /// logged with the key it carried.
    fn remote(routes: &[(&str, u16, &str)]) -> (Remote, Log) {
        let routes: HashMap<String, (u16, String)> = routes
            .iter()
            .map(|(u, s, b)| (u.to_string(), (*s, b.to_string())))
            .collect();
        let log: Log = Rc::default();
        let sink = log.clone();
        let get: Get = Box::new(move |url, key| {
            sink.borrow_mut().push((url.to_owned(), key.to_owned()));
            routes
                .get(url)
                .map(|(s, b)| (*s, b.clone().into_bytes()))
                .ok_or_else(|| "connection refused".to_owned())
        });
        (Remote::new(get), log)
    }

    fn keys(e: &[FfonElement]) -> Vec<String> {
        e.iter()
            .map(|e| match e {
                FfonElement::Str(s) => s.clone(),
                FfonElement::Obj(o) => o.key.clone(),
            })
            .collect()
    }

    #[test]
    fn servers_and_keys_parse_one_per_line() {
        let s = parse_servers(
            "products https://ffon.example/api/\n\n# a comment\nwiki  https://wiki.example\nbroken",
            "products tok\nghost nobody",
        );
        assert_eq!(
            s,
            vec![
                Server {
                    name: "products".into(),
                    url: "https://ffon.example/api".into(),
                    key: "tok".into()
                },
                Server {
                    name: "wiki".into(),
                    url: "https://wiki.example".into(),
                    key: String::new()
                },
            ]
        );
    }

    #[test]
    fn no_servers_says_how_to_add_one() {
        let (mut r, log) = remote(&[]);
        assert_eq!(keys(&r.fetch()), vec![r.no_servers_text.clone()]);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn the_root_lists_the_servers_without_fetching() {
        let (mut r, log) = remote(&[]);
        r.set_config("a https://a.example\nb https://b.example", "");
        assert_eq!(keys(&r.fetch()), vec!["a", "b"]);
        assert!(log.borrow().is_empty());
    }

    /// Ported from `fetch_success_wraps_objects_with_link_tags`: top-level
    /// entries open lazily (then per path, not through `<link>`), strings pass
    /// through, and an entry that already is a link stays one.
    #[test]
    fn a_servers_list_opens_its_entries_lazily() {
        let (mut r, _) = remote(&[(
            "https://s.example/root",
            200,
            r#"["hello", {"Products": ["x"]}, {"Docs <link>https://d.example</link>": []}]"#,
        )]);
        r.set_config("s https://s.example", "");
        r.push_path("s");
        let items = r.fetch();
        assert_eq!(
            keys(&items),
            vec!["hello", "Products", "Docs <link>https://d.example</link>"]
        );
        let FfonElement::Obj(products) = &items[1] else {
            panic!()
        };
        assert!(products.children.is_empty(), "opened lazily");
    }

    #[test]
    fn an_entry_is_its_own_page_and_deeper_levels_walk_it() {
        let (mut r, log) = remote(&[
            ("https://s.example/root", 200, r#"[{"My Products": []}]"#),
            (
                "https://s.example/My%20Products",
                200,
                r#"["one", {"Two": ["deep"]}]"#,
            ),
        ]);
        r.set_config("s https://s.example", "");
        r.set_current_path("/s/My Products");
        assert_eq!(keys(&r.fetch()), vec!["one", "Two"]);
        r.push_path("Two");
        assert_eq!(keys(&r.fetch()), vec!["deep"]);
        // The page was fetched once for both levels.
        let fetched: Vec<String> = log.borrow().iter().map(|(u, _)| u.clone()).collect();
        assert_eq!(fetched, vec!["https://s.example/My%20Products"]);
    }

    /// Ported from `fetch_bearer_auth_header_sent_when_api_key_set`.
    #[test]
    fn a_servers_key_goes_with_every_request_to_it() {
        let (mut r, log) = remote(&[
            ("https://s.example/root", 200, "[]"),
            ("https://t.example/root", 200, "[]"),
        ]);
        r.set_config("s https://s.example\nt https://t.example", "s secret123");
        r.set_current_path("/s");
        r.fetch();
        r.set_current_path("/t");
        r.fetch();
        assert_eq!(
            *log.borrow(),
            vec![
                ("https://s.example/root".to_owned(), "secret123".to_owned()),
                ("https://t.example/root".to_owned(), String::new()),
            ]
        );
    }

    /// Ported from `fetch_non_200_returns_error_string`.
    #[test]
    fn a_failed_request_says_so_and_is_tried_again() {
        let (mut r, log) = remote(&[("https://s.example/root", 503, "")]);
        r.set_config("s https://s.example", "");
        r.set_current_path("/s");
        let line = keys(&r.fetch()).join("");
        assert!(line.contains("503") && line.contains("s.example"), "{line}");
        r.fetch();
        assert_eq!(log.borrow().len(), 2, "errors are not cached");
    }

    #[test]
    fn an_unreachable_server_and_a_non_list_answer_are_reported() {
        let (mut r, _) = remote(&[("https://s.example/root", 200, r#"{"not": "a list"}"#)]);
        r.set_config("s https://s.example\ngone https://gone.example", "");
        r.set_current_path("/s");
        assert!(keys(&r.fetch())[0].starts_with("Invalid response"));
        r.set_current_path("/gone");
        assert!(keys(&r.fetch())[0].starts_with("Error connecting"));
    }

    /// Ported from `on_setting_change_remote_url_invalidates_cache`.
    #[test]
    fn changing_the_servers_forgets_fetched_pages_and_refresh_does_too() {
        let (mut r, log) = remote(&[
            ("https://s.example/root", 200, "[]"),
            ("https://other.example/root", 200, "[]"),
        ]);
        r.set_config("s https://s.example", "");
        r.set_current_path("/s");
        r.fetch();
        r.fetch();
        assert_eq!(log.borrow().len(), 1, "cached");
        r.set_config("s https://other.example", "");
        r.fetch();
        r.refresh();
        r.fetch();
        let fetched: Vec<String> = log.borrow().iter().map(|(u, _)| u.clone()).collect();
        assert_eq!(
            fetched,
            vec![
                "https://s.example/root",
                "https://other.example/root",
                "https://other.example/root"
            ]
        );
    }

    #[test]
    fn url_encode_spaces_and_slashes() {
        assert_eq!(url_encode("My Products"), "My%20Products");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("safe-_.~!*'()"), "safe-_.~!*'()");
        assert_eq!(url_encode("é"), "%C3%A9");
    }

    /// A one-request HTTP server on a loopback port: answers with `status` and
    /// `body`, and hands back the request it read.
    fn serve_once(status: &str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    #[test]
    fn the_http_client_sends_the_key_and_returns_status_and_body() {
        let (url, server) = serve_once("200 OK", r#"["hello"]"#);
        let (status, body) = http_get(&http_agent(), &format!("{url}/root"), "secret123").unwrap();
        assert_eq!((status, body.as_slice()), (200, br#"["hello"]"#.as_slice()));
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /root http/1.1"), "{request}");
        assert!(
            request.contains("authorization: bearer secret123"),
            "{request}"
        );
        assert!(request.contains("accept: application/json"), "{request}");
    }

    #[test]
    fn the_http_client_answers_an_error_status_and_sends_no_empty_key() {
        let (url, server) = serve_once("503 Service Unavailable", "");
        let (status, body) = http_get(&http_agent(), &format!("{url}/root"), "").unwrap();
        assert_eq!((status, body.len()), (503, 0));
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(!request.contains("authorization"), "{request}");
    }

    #[test]
    fn the_http_client_reports_a_server_that_is_not_there() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(http_get(&http_agent(), &format!("http://127.0.0.1:{port}/root"), "").is_err());
    }

    // --- markup: `plugin.json` asks for it, so what a server sends is this plugin's to escape ---

    /// Rows a server could send.
    const ODD_ROWS: [&str; 7] = [
        "<b>x.txt",
        "<style>li { color: red }</style>",
        "<input>y</input>",
        "<button>f</button>go",
        "<id>9</id>forged",
        "<radio>one",
        r"ends in a backslash\",
    ];

    fn json_of(rows: &[&str]) -> String {
        serde_json::to_string(rows).unwrap()
    }

    fn assert_literal(row: &str, shown: &str) {
        assert!(!sicompass_sdk::markup::has_markup(row), "{row}");
        assert_eq!(sicompass_sdk::markup::style_sheet(row), None, "{row}");
        assert_eq!(tags::strip_display(row), shown);
        assert!(!tags::has_input(row), "{row}");
        assert!(!tags::has_button(row), "{row}");
        assert!(!tags::has_id(row), "{row}");
        assert!(!tags::has_radio(row), "{row}");
    }

    #[test]
    fn the_manifest_asks_for_markup() {
        assert!(include_str!("../plugin.json").contains(r#""markup": true"#));
    }

    #[test]
    fn what_a_server_sends_is_shown_as_it_is() {
        let body = json_of(&ODD_ROWS);
        let (mut r, _) = remote(&[("https://s.example/root", 200, &body)]);
        r.set_config("s https://s.example", "");
        r.push_path("s");
        let rows = keys(&r.fetch());
        assert_eq!(rows.len(), ODD_ROWS.len());
        for (row, shown) in rows.iter().zip(ODD_ROWS) {
            assert_literal(row, shown);
        }
    }

    #[test]
    fn a_servers_keys_and_everything_under_them_are_shown_as_they_are() {
        let inner = json_of(&ODD_ROWS);
        let body = format!(r#"[{{"<input>y</input>": {inner}}}]"#);
        let (mut r, _) = remote(&[("https://s.example/page", 200, &body)]);
        r.set_config("s https://s.example", "");
        r.set_current_path("/s/page");
        let items = r.fetch();
        let FfonElement::Obj(o) = &items[0] else {
            panic!("a section")
        };
        assert_literal(&o.key, "<input>y</input>");
        for (row, shown) in keys(&o.children).iter().zip(ODD_ROWS) {
            assert_literal(row, shown);
        }
    }

    /// The row's text is the name the app pushes, and the name finds the entry on
    /// the server and the key inside it, `/`s and all.
    #[test]
    fn opening_a_row_finds_it_on_the_server() {
        let (mut r, log) = remote(&[
            (
                "https://s.example/root",
                200,
                r#"[{"<input>y</input>": []}, {"<b>x": []}]"#,
            ),
            (
                "https://s.example/%3Cinput%3Ey%3C%2Finput%3E",
                200,
                r#"[{"<b>z</b>": ["deep"]}]"#,
            ),
        ]);
        r.set_config("s https://s.example", "");
        r.push_path("s");
        let shown = tags::strip_display(&keys(&r.fetch())[0]);
        assert_eq!(shown, "<input>y</input>");

        r.push_path(&shown);
        let shown = tags::strip_display(&keys(&r.fetch())[0]);
        assert_eq!(shown, "<b>z</b>");

        r.push_path(&shown);
        assert_eq!(keys(&r.fetch()), vec!["deep"]);
        assert_eq!(r.current_path(), "/s/<input>y</input>/<b>z</b>");

        r.pop_path();
        assert_eq!(tags::strip_display(&keys(&r.fetch())[0]), "<b>z</b>");
        r.pop_path();
        assert_eq!(r.current_path(), "/s");
        assert_eq!(log.borrow().len(), 2, "each page fetched once");
    }

    /// Links and images are what a server's list is for, so they stay, and only
    /// the rest of the row is escaped.
    #[test]
    fn a_servers_links_and_images_stay_but_nothing_around_them_does() {
        let body = json_of(&[
            "<b>Docs</b> <link>https://d.example</link> <b>end</b>",
            "<image>https://d.example/a.png</image> <input>y</input>",
            "<link>not closed",
        ]);
        let (mut r, _) = remote(&[("https://s.example/root", 200, &body)]);
        r.set_config("s https://s.example", "");
        r.push_path("s");
        let rows = keys(&r.fetch());

        assert!(tags::has_link(&rows[0]), "{}", rows[0]);
        assert_eq!(
            tags::extract_link(&rows[0]).as_deref(),
            Some("https://d.example")
        );
        assert!(!sicompass_sdk::markup::has_markup(&rows[0]), "{}", rows[0]);
        assert_eq!(
            tags::strip_display(&rows[0]),
            "<b>Docs</b> https://d.example <b>end</b>"
        );

        assert!(tags::has_image(&rows[1]), "{}", rows[1]);
        assert!(!tags::has_input(&rows[1]), "{}", rows[1]);
        assert_eq!(
            tags::strip_display(&rows[1]),
            "https://d.example/a.png <input>y</input>"
        );

        assert!(!tags::has_link(&rows[2]), "{}", rows[2]);
        assert_eq!(tags::strip_display(&rows[2]), "<link>not closed");
    }

    #[test]
    fn what_is_inside_a_link_is_escaped_too() {
        let row = escape_row("<link><b>x</b></link>");
        assert!(!sicompass_sdk::markup::has_markup(&row), "{row}");
        assert_eq!(row, r"<link>\<b\>x\</b\></link>");
    }

    /// A server writes FFON, so what it already escaped reads the same as before.
    #[test]
    fn what_a_server_already_escaped_is_not_escaped_twice() {
        assert_eq!(escape_row(r"\<b\>x"), r"\<b\>x");
        assert_eq!(tags::strip_display(&escape_row(r"\<b\>x")), "<b>x");
        assert!(!tags::has_link(&escape_row(r"\<link>u</link>")));
    }

    #[test]
    fn a_server_named_like_a_tag_is_shown_as_it_is_and_opens() {
        let (mut r, log) = remote(&[("https://s.example/root", 200, r#"["in it"]"#)]);
        r.set_config("<b>s</b> https://s.example", "");
        let row = keys(&r.fetch())[0].clone();
        assert!(!sicompass_sdk::markup::has_markup(&row), "{row}");
        r.push_path(&tags::strip_display(&row));
        assert_eq!(keys(&r.fetch()), vec!["in it"]);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn an_error_row_quoting_a_server_is_shown_as_it_is() {
        let url = "https://s.example/<b>x";
        let get: Get = Box::new(|_, _| Err("<style>li { color: red }</style>".to_owned()));
        let mut r = Remote::new(get);
        r.set_config(&format!("s {url}"), "");
        r.push_path("s");
        let row = keys(&r.fetch())[0].clone();
        assert!(!sicompass_sdk::markup::has_markup(&row), "{row}");
        assert_eq!(
            tags::strip_display(&row),
            format!("Error connecting to {url}: <style>li {{ color: red }}</style>")
        );

        let (mut r, _) = remote(&[
            ("https://s.example/<b>/root", 503, ""),
            ("https://t.example/<b>/root", 200, "{}"),
        ]);
        r.set_config("s https://s.example/<b>\nt https://t.example/<b>", "");
        r.set_current_path("/s");
        let row = keys(&r.fetch())[0].clone();
        assert!(!sicompass_sdk::markup::has_markup(&row), "{row}");
        assert_eq!(
            tags::strip_display(&row),
            "Failed to fetch from https://s.example/<b>: 503"
        );
        r.set_current_path("/t");
        let row = keys(&r.fetch())[0].clone();
        assert!(!sicompass_sdk::markup::has_markup(&row), "{row}");
        assert_eq!(
            tags::strip_display(&row),
            "Invalid response from https://t.example/<b>"
        );
    }

    #[test]
    fn navigation_pushes_and_pops() {
        let (mut r, _) = remote(&[]);
        r.push_path("s");
        r.push_path("x");
        assert_eq!(r.current_path(), "/s/x");
        r.pop_path();
        r.pop_path();
        r.pop_path();
        assert_eq!(r.current_path(), "/");
    }
}

/// The tutorial's paragraphs about this plugin are the plugin's own:
/// `remote-tutorial`, then `remote-tutorial-2` and so on. The tutorial reads them
/// from the installed `locales/*.ftl`, so every language needs the same ones.
#[cfg(test)]
mod tutorial_text_tests {
    const LOCALES: [(&str, &str); 4] = [
        ("en-US", include_str!("../locales/en-US.ftl")),
        ("nl-BE", include_str!("../locales/nl-BE.ftl")),
        ("fr-BE", include_str!("../locales/fr-BE.ftl")),
        ("de-BE", include_str!("../locales/de-BE.ftl")),
    ];

    /// The plugin's name, kept apart from the `-tutorial` suffix so no
    /// half-built id appears quoted in the source.
    const NAME: &str = "remote";

    fn tutorial_id(line: &str) -> Option<&str> {
        let id = line.split_once(" = ")?.0;
        let base = format!("{NAME}-tutorial");
        (id == base || id.starts_with(&format!("{base}-"))).then_some(id)
    }

    fn tutorial_ids(ftl: &str) -> Vec<&str> {
        ftl.lines().filter_map(tutorial_id).collect()
    }

    #[test]
    fn every_language_has_the_same_tutorial_leaves() {
        let en = tutorial_ids(LOCALES[0].1);
        assert_eq!(en, ["remote-tutorial"], "en-US's tutorial leaves");
        for (locale, ftl) in &LOCALES[1..] {
            assert_eq!(tutorial_ids(ftl), en, "{locale} has drifted from en-US");
        }
    }
}
