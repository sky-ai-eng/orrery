//! Where the face's surfaces are, so every URL it returns is built from one
//! place (spec/grammar.md, "Where the face is").

/// The roots every URL the face renders starts from. A URL is a root followed
/// by a path that starts with `/`.
#[derive(Clone, Debug)]
pub struct Layout {
    api: String,
    web: String,
    web_host: String,
}

impl Layout {
    /// GitHub Enterprise Server's layout at `base_url`: REST under
    /// `{base_url}/api/v3`, and web pages and git at `base_url` itself.
    pub fn enterprise_server(base_url: &str) -> Layout {
        let web = base_url.trim_end_matches('/').to_string();
        Layout {
            api: format!("{web}/api/v3"),
            web_host: host_name(&web),
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
}
