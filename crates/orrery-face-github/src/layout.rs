//! Where the face's surfaces are, and by which host a request finds its
//! surface (spec/grammar.md, "Where the face is").

/// Where GitHub Enterprise Server serves REST, below its base URL. The REST
/// face is mounted here.
pub const REST_PATH: &str = "/api/v3";

/// Where GitHub Enterprise Server serves GraphQL: beside REST, not under it.
pub const GRAPHQL_PATH: &str = "/api/graphql";

/// A REST route's path where the router mounts it: `path` below
/// [`REST_PATH`], for a route mounted beside the REST face rather than in it.
pub fn rest(path: &str) -> String {
    format!("{REST_PATH}{path}")
}

/// github.com's REST and GraphQL host.
pub const GITHUB_API_HOST: &str = "api.github.com";
/// github.com's web host, which serves git, the OAuth web flow, and the
/// GitHub App pages.
pub const GITHUB_WEB_HOST: &str = "github.com";

/// The roots of the URLs the face renders. A URL is a root followed by a
/// path that starts with `/`.
#[derive(Clone, Debug)]
pub struct Layout {
    kind: Kind,
    api: String,
    web: String,
    web_host: String,
    web_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    EnterpriseServer,
    GitHubCom,
}

/// Where a request is served, by the host it names.
#[derive(Debug, PartialEq, Eq)]
pub enum Landing {
    /// The world's own host: every surface at its GitHub Enterprise Server
    /// path, as the request names it.
    Own,
    /// github.com's API host. The request is served at this path and query,
    /// its GitHub Enterprise Server address.
    Api(String),
    /// github.com's web host, whose web pages and git are at the world's own
    /// paths, and which serves neither REST nor GraphQL.
    Web,
    /// A GitHub host the face does not serve, or REST or GraphQL on
    /// github.com's web host.
    NotServed,
}

impl Layout {
    /// GitHub Enterprise Server's layout at `base_url`: REST under
    /// `{base_url}/api/v3`, and web pages and git at `base_url` itself.
    pub fn enterprise_server(base_url: &str) -> Layout {
        let web = base_url.trim_end_matches('/').to_string();
        let uri = web.parse::<axum::http::Uri>().ok();
        Layout {
            kind: Kind::EnterpriseServer,
            api: format!("{web}{REST_PATH}"),
            web_host: uri
                .as_ref()
                .and_then(|uri| uri.host().map(str::to_string))
                .unwrap_or_default(),
            web_path: uri
                .as_ref()
                .map(|uri| uri.path().trim_end_matches('/').to_string())
                .unwrap_or_default(),
            web,
        }
    }

    /// github.com's layout: REST at the root of `https://api.github.com`,
    /// and web pages and git at `https://github.com`.
    // TODO(ORR-123): no listener serves these `https://` URLs yet; a client
    // reaches the world as github.com's hosts only over plain HTTP with the
    // `Host` header set until the world terminates TLS.
    pub fn github_com() -> Layout {
        Layout {
            kind: Kind::GitHubCom,
            api: format!("https://{GITHUB_API_HOST}"),
            web: format!("https://{GITHUB_WEB_HOST}"),
            web_host: GITHUB_WEB_HOST.to_string(),
            web_path: String::new(),
        }
    }

    /// Whether this is github.com's layout, under which a request finds its
    /// surface by the host it names.
    pub fn is_github_com(&self) -> bool {
        self.kind == Kind::GitHubCom
    }

    /// The REST root, with no trailing slash.
    pub fn api(&self) -> &str {
        &self.api
    }

    /// The root of web pages and git, with no trailing slash.
    pub fn web(&self) -> &str {
        &self.web
    }

    /// The web root's host name, without its scheme, port, or path.
    pub fn web_host(&self) -> &str {
        &self.web_host
    }

    /// The web root's path, with no trailing slash: empty unless the base
    /// URL has one. A page's form posts to this path followed by the route's,
    /// on whichever host served the page, so the form works through any host
    /// that reaches the world.
    pub fn web_path(&self) -> &str {
        &self.web_path
    }

    /// Where a request for `host` and `path_and_query` is served. `host` is
    /// a name, with or without a port, in any case, and with or without the
    /// trailing dot of a fully qualified name. Under GitHub Enterprise
    /// Server's layout every request is the world's own, whatever host it
    /// names.
    pub fn landing(&self, host: Option<&str>, path_and_query: &str) -> Landing {
        if self.kind == Kind::EnterpriseServer {
            return Landing::Own;
        }
        let Some(host) = host.map(host_name_of) else {
            return Landing::Own;
        };
        let path = path_and_query.split('?').next().unwrap_or_default();
        if host.eq_ignore_ascii_case(GITHUB_API_HOST) {
            // GraphQL is a sibling of the REST root on GitHub Enterprise
            // Server, and on github.com it is a path under it.
            let served = if path == "/graphql" {
                path_and_query.replacen("/graphql", GRAPHQL_PATH, 1)
            } else {
                format!("{REST_PATH}{path_and_query}")
            };
            return Landing::Api(served);
        }
        if host.eq_ignore_ascii_case(GITHUB_WEB_HOST) {
            return if under(path, REST_PATH) || under(path, GRAPHQL_PATH) {
                Landing::NotServed
            } else {
                Landing::Web
            };
        }
        if is_github_host(host) {
            return Landing::NotServed;
        }
        Landing::Own
    }
}

/// Whether `path` is `prefix` or a path below it.
pub fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether `host`, a name with or without a port and a trailing dot, is in
/// one of GitHub's own domains, whose hosts the face under github.com's
/// layout either serves or answers as not served, so a request for one never
/// lands on the world's own host.
pub fn is_github_host(host: &str) -> bool {
    let host = host_name_of(host);
    ["github.com", "githubusercontent.com"]
        .iter()
        .any(|domain| {
            let Some(start) = host.len().checked_sub(domain.len()) else {
                return false;
            };
            host.is_char_boundary(start)
                && host[start..].eq_ignore_ascii_case(domain)
                && (start == 0 || host[..start].ends_with('.'))
        })
}

/// A `Host` value's name: `api.github.com:443` and `api.github.com.` are
/// `api.github.com`, and `[::1]:4180` is `[::1]`.
fn host_name_of(host: &str) -> &str {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name.strip_suffix('.').unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enterprise_server_roots() {
        for base in ["http://127.0.0.1:4180", "http://127.0.0.1:4180/"] {
            let layout = Layout::enterprise_server(base);
            assert_eq!(layout.api(), "http://127.0.0.1:4180/api/v3");
            assert_eq!(layout.web(), "http://127.0.0.1:4180");
            assert_eq!(layout.web_host(), "127.0.0.1");
            assert_eq!(layout.web_path(), "");
        }
        let layout = Layout::enterprise_server("https://world.example/prefix");
        assert_eq!(layout.api(), "https://world.example/prefix/api/v3");
        assert_eq!(layout.web(), "https://world.example/prefix");
        assert_eq!(layout.web_host(), "world.example");
        assert_eq!(layout.web_path(), "/prefix");
    }

    #[test]
    fn github_com_roots() {
        let layout = Layout::github_com();
        assert_eq!(layout.api(), "https://api.github.com");
        assert_eq!(layout.web(), "https://github.com");
        assert_eq!(layout.web_host(), "github.com");
        assert_eq!(layout.web_path(), "");
    }

    #[test]
    fn enterprise_server_serves_every_host_as_its_own() {
        let layout = Layout::enterprise_server("http://127.0.0.1:4180");
        for host in [None, Some("api.github.com"), Some("github.com")] {
            assert_eq!(layout.landing(host, "/repos/a/b"), Landing::Own);
        }
    }

    #[test]
    fn github_com_lands_by_host() {
        let layout = Layout::github_com();
        let api = |p: &str| Landing::Api(p.to_string());
        let cases = [
            (
                Some("api.github.com"),
                "/repos/a/b",
                api("/api/v3/repos/a/b"),
            ),
            (
                Some("API.GitHub.com:443"),
                "/user?x=1",
                api("/api/v3/user?x=1"),
            ),
            (
                Some("api.github.com."),
                "/repos/a/b",
                api("/api/v3/repos/a/b"),
            ),
            (Some("api.github.com.:443"), "/user", api("/api/v3/user")),
            (Some("api.github.com"), "/graphql", api("/api/graphql")),
            (
                Some("api.github.com"),
                "/graphql?x=1",
                api("/api/graphql?x=1"),
            ),
            (Some("api.github.com"), "/graphqlx", api("/api/v3/graphqlx")),
            (Some("api.github.com"), "/", api("/api/v3/")),
            (Some("github.com"), "/a/b.git/info/refs", Landing::Web),
            (
                Some("github.com:443"),
                "/login/oauth/authorize",
                Landing::Web,
            ),
            (Some("github.com."), "/a/b.git/info/refs", Landing::Web),
            (Some("github.com"), "/api/v3/repos/a/b", Landing::NotServed),
            (Some("github.com."), "/api/v3/repos/a/b", Landing::NotServed),
            (Some("github.com"), "/api/v3", Landing::NotServed),
            (Some("github.com"), "/api/graphql", Landing::NotServed),
            (Some("github.com"), "/api/v3x/a", Landing::Web),
            (
                Some("codeload.github.com"),
                "/a/b/tar.gz/main",
                Landing::NotServed,
            ),
            (
                Some("CODELOAD.GitHub.com."),
                "/a/b/tar.gz/main",
                Landing::NotServed,
            ),
            (
                Some("raw.githubusercontent.com"),
                "/a/b/main/f",
                Landing::NotServed,
            ),
            (Some("notgithub.com"), "/api/v3/repos/a/b", Landing::Own),
            (Some("github.com.evil"), "/api/v3/repos/a/b", Landing::Own),
            (Some("127.0.0.1:4180"), "/api/v3/repos/a/b", Landing::Own),
            (Some("[::1]:4180"), "/api/v3/repos/a/b", Landing::Own),
            (None, "/api/v3/repos/a/b", Landing::Own),
        ];
        for (host, path, landing) in cases {
            assert_eq!(layout.landing(host, path), landing, "{host:?} {path}");
        }
    }
}
