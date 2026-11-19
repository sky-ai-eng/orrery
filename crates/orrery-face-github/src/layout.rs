//! Where the face's surfaces are, so every URL it returns is built from one
//! place, and every request finds its surface by one rule (spec/grammar.md,
//! "Where the face is").

/// The roots every URL the face renders starts from. A URL is a root followed
/// by a path that starts with `/`.
#[derive(Clone, Debug)]
pub struct Layout {
    kind: Kind,
    api: String,
    web: String,
    web_host: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    EnterpriseServer,
    GitHubCom,
}

/// github.com's REST and GraphQL host.
pub const GITHUB_API_HOST: &str = "api.github.com";
/// github.com's web host, which serves git, the OAuth web flow, and the
/// GitHub App pages.
pub const GITHUB_WEB_HOST: &str = "github.com";

/// Where a request is served, by the host it names.
#[derive(Debug, PartialEq, Eq)]
pub enum Landing {
    /// The world's own host: every surface at its GitHub Enterprise Server
    /// path, as the request names it.
    Own,
    /// github.com's API host. The request is served at this path and query,
    /// its GitHub Enterprise Server address.
    Api(String),
    /// github.com's web host, whose paths are the world's own.
    Web,
    /// A GitHub host the face does not serve, or a path GitHub does not
    /// serve on a host the face does.
    NotServed,
}

impl Layout {
    /// GitHub Enterprise Server's layout at `base_url`: REST under
    /// `{base_url}/api/v3`, and web pages and git at `base_url` itself.
    pub fn enterprise_server(base_url: &str) -> Layout {
        let web = base_url.trim_end_matches('/').to_string();
        Layout {
            kind: Kind::EnterpriseServer,
            api: format!("{web}/api/v3"),
            web_host: host_name(&web),
            web,
        }
    }

    /// github.com's layout: REST at the root of `https://api.github.com`,
    /// and web pages and git at `https://github.com`.
    pub fn github_com() -> Layout {
        Layout {
            kind: Kind::GitHubCom,
            api: format!("https://{GITHUB_API_HOST}"),
            web: format!("https://{GITHUB_WEB_HOST}"),
            web_host: GITHUB_WEB_HOST.to_string(),
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

    /// Where a request for `host` (its name, with or without a port) and
    /// `path_and_query` is served. Under GitHub Enterprise Server's layout
    /// every request is the world's own, whatever host it names.
    pub fn landing(&self, host: Option<&str>, path_and_query: &str) -> Landing {
        if self.kind == Kind::EnterpriseServer {
            return Landing::Own;
        }
        let Some(host) = host.map(host_without_port) else {
            return Landing::Own;
        };
        let path = path_and_query.split('?').next().unwrap_or_default();
        if host.eq_ignore_ascii_case(GITHUB_API_HOST) {
            // GraphQL is a sibling of the REST root on GitHub Enterprise
            // Server, and on github.com it is a path under it.
            let served = if path == "/graphql" {
                path_and_query.replacen("/graphql", "/api/graphql", 1)
            } else {
                format!("/api/v3{path_and_query}")
            };
            return Landing::Api(served);
        }
        if host.eq_ignore_ascii_case(GITHUB_WEB_HOST) {
            // The world's own host carries REST, GraphQL, and the control
            // plane beside the web paths; github.com carries none of them.
            let own_only = ["/api/v3", "/api/graphql", "/_orrery"]
                .iter()
                .any(|prefix| under(path, prefix));
            return if own_only {
                Landing::NotServed
            } else {
                Landing::Web
            };
        }
        if is_github_domain(host) {
            return Landing::NotServed;
        }
        Landing::Own
    }
}

/// Whether `path` is `prefix` or a path below it.
fn under(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// GitHub's own domains, whose hosts the face either serves or answers as
/// not served, so a request for one never lands on the world's own host.
fn is_github_domain(host: &str) -> bool {
    ["github.com", "githubusercontent.com"]
        .iter()
        .any(|domain| {
            let host = host.to_ascii_lowercase();
            host == *domain || host.ends_with(&format!(".{domain}"))
        })
}

/// A `Host` value's name: `api.github.com:443` is `api.github.com`, and
/// `[::1]:4180` is `[::1]`.
fn host_without_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    }
}

fn host_name(base: &str) -> String {
    base.parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_string))
        .unwrap_or_default()
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
        }
        let layout = Layout::enterprise_server("https://world.example/prefix");
        assert_eq!(layout.api(), "https://world.example/prefix/api/v3");
        assert_eq!(layout.web(), "https://world.example/prefix");
        assert_eq!(layout.web_host(), "world.example");
    }

    #[test]
    fn github_com_roots() {
        let layout = Layout::github_com();
        assert_eq!(layout.api(), "https://api.github.com");
        assert_eq!(layout.web(), "https://github.com");
        assert_eq!(layout.web_host(), "github.com");
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
            (Some("github.com"), "/api/v3/repos/a/b", Landing::NotServed),
            (Some("github.com"), "/api/v3", Landing::NotServed),
            (Some("github.com"), "/api/graphql", Landing::NotServed),
            (Some("github.com"), "/_orrery/healthz", Landing::NotServed),
            (Some("github.com"), "/api/v3x/a", Landing::Web),
            (
                Some("codeload.github.com"),
                "/a/b/tar.gz/main",
                Landing::NotServed,
            ),
            (
                Some("raw.githubusercontent.com"),
                "/a/b/main/f",
                Landing::NotServed,
            ),
            (Some("notgithub.com"), "/api/v3/repos/a/b", Landing::Own),
            (Some("127.0.0.1:4180"), "/api/v3/repos/a/b", Landing::Own),
            (Some("[::1]:4180"), "/api/v3/repos/a/b", Landing::Own),
            (None, "/api/v3/repos/a/b", Landing::Own),
        ];
        for (host, path, landing) in cases {
            assert_eq!(layout.landing(host, path), landing, "{host:?} {path}");
        }
    }
}
