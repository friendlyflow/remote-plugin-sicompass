//! Remote, a sicompass WASM plugin: browse FFON served over HTTP.
//!
//! A server answers `GET <url>/root` with a JSON FFON array, and `GET
//! <url>/<entry>` with the children of a top-level entry. Remote shows each
//! server the user configured as a section of its own, fetches a level when it
//! is opened, and sends the server's API key as a bearer token.
//!
//! The servers are the user's, so the plugin asks for any server
//! (`"allowedHosts": ["*"]`), which the Store shows before install and the user
//! approves. The host still refuses internal addresses and honours robots.txt.
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
//! tested natively; [`RemotePlugin`] connects it to the plugin interface.

use std::collections::HashMap;

use sicompass_pdk::{Descriptor, Plugin, export_plugin, host, net};
use sicompass_sdk::ffon::{FfonElement, parse_json_value};

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

    pub fn set_current_path(&mut self, path: &str) {
        self.path = path.to_owned();
    }

    pub fn push_path(&mut self, segment: &str) {
        if self.path == "/" {
            self.path = format!("/{segment}");
        } else {
            self.path.push('/');
            self.path.push_str(segment);
        }
    }

    pub fn pop_path(&mut self) {
        match self.path.rfind('/') {
            Some(0) | None => self.path = "/".to_owned(),
            Some(i) => self.path.truncate(i),
        }
    }

    pub fn fetch(&mut self) -> Vec<FfonElement> {
        let parts: Vec<String> = self
            .path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let Some((name, rest)) = parts.split_first() else {
            if self.servers.is_empty() {
                return vec![FfonElement::new_str(self.no_servers_text.clone())];
            }
            return self
                .servers
                .iter()
                .map(|s| FfonElement::new_obj(s.name.clone()))
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
                        FfonElement::Obj(o) if !sicompass_sdk::tags::has_link(&o.key) => {
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
                        FfonElement::Obj(o)
                            if sicompass_sdk::tags::strip_display(&o.key) == segment.as_str() =>
                        {
                            Some(o.children)
                        }
                        _ => None,
                    });
                    level = next.unwrap_or_default();
                }
                level
            }),
        };
        result.unwrap_or_else(|e| vec![FfonElement::new_str(e)])
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
        let elements: Vec<FfonElement> = items.iter().map(parse_json_value).collect();
        self.cache.insert(url, elements.clone());
        Ok(elements)
    }
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

fn net_get(url: &str, key: &str) -> Result<(u16, Vec<u8>), String> {
    let mut headers = vec![("Accept".to_owned(), "application/json".to_owned())];
    if !key.is_empty() {
        headers.push(("Authorization".to_owned(), format!("Bearer {key}")));
    }
    let resp = net::fetch(&net::HttpRequest {
        method: "GET".to_owned(),
        url: url.to_owned(),
        headers,
        body: None,
    })?;
    Ok((resp.status, resp.body))
}

impl Plugin for RemotePlugin {
    fn new() -> Self {
        RemotePlugin {
            remote: Remote::new(Box::new(net_get)),
        }
    }

    fn init(&mut self) {
        self.remote.no_servers_text = host::translate("remote-no-servers");
        self.load_config();
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

export_plugin!(RemotePlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

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
