//! Where the face's surfaces are (spec/grammar.md, "Where the face is").

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

/// The roots of the URLs the face renders. A URL is a root followed by a
/// path that starts with `/`.
#[derive(Clone, Debug)]
pub struct Layout {
    api: String,
    web: String,
    web_host: String,
    web_path: String,
}

impl Layout {
    /// GitHub Enterprise Server's layout at `base_url`: REST under
    /// `{base_url}/api/v3`, and web pages and git at `base_url` itself.
    pub fn enterprise_server(base_url: &str) -> Layout {
        let web = base_url.trim_end_matches('/').to_string();
        let uri = web.parse::<axum::http::Uri>().ok();
        Layout {
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
}
