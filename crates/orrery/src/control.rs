//! The CLI's side of a world's control plane: how a command addresses one
//! world, and the calls it issues there.

use anyhow::Context as _;
use ureq::http::Uri;

/// Which world a command talks to, named by its control listener's origin.
#[derive(clap::Args)]
pub struct Address {
    /// The world's control listener, as an http or https origin without
    /// `/_orrery`. The default is `serve`'s default listen.
    #[arg(
        long = "control-url",
        env = "ORRERY_CONTROL_URL",
        default_value = "http://127.0.0.1:4180",
        value_parser = parse_origin,
    )]
    origin: String,
}

/// Seed scripts reach a world over TCP, because `git` cannot push to a Unix
/// socket, so only http and https are accepted. A path is refused rather than
/// joined to `/_orrery`, so a URL that already ends in `/_orrery` fails here
/// with a reason instead of at the world with a 501.
fn parse_origin(raw: &str) -> Result<String, String> {
    let fault = |what: &str| {
        format!("{what}; expected an http or https origin, like http://127.0.0.1:4180")
    };
    let uri: Uri = raw.parse().map_err(|_| fault("not a URL"))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(fault("the scheme is not http or https"));
    }
    let Some(authority) = uri.authority() else {
        return Err(fault("no host"));
    };
    if uri.path() != "/" || uri.query().is_some() {
        return Err(fault("a path or query after the host (leave out /_orrery)"));
    }
    Ok(format!("{scheme}://{authority}"))
}

#[derive(serde::Deserialize)]
struct Clock {
    clock_unix: i64,
}

impl Address {
    /// `GET /_orrery/clock`: what time the world believes it is.
    pub fn clock(&self) -> anyhow::Result<i64> {
        let body = self.get("/_orrery/clock")?;
        let clock: Clock = serde_json::from_str(&body)
            .with_context(|| format!("{}/_orrery/clock answered {body}", self.origin))?;
        Ok(clock.clock_unix)
    }

    /// The body of a 2xx answer; any other answer is an error carrying it.
    fn get(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}{path}", self.origin);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .call()
            .with_context(|| format!("reaching the world at {}", self.origin))?;
        let status = response.status();
        let body = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("reading the answer from {url}"))?;
        anyhow::ensure!(status.is_success(), "GET {url} answered {status}: {body}");
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_origin;

    #[test]
    fn an_origin_is_accepted_with_or_without_a_trailing_slash() {
        for raw in ["http://127.0.0.1:4180", "http://127.0.0.1:4180/"] {
            assert_eq!(parse_origin(raw).unwrap(), "http://127.0.0.1:4180");
        }
        assert_eq!(
            parse_origin("https://world.example").unwrap(),
            "https://world.example"
        );
    }

    #[test]
    fn anything_but_an_http_origin_is_refused() {
        for raw in [
            "unix:/run/orrery/control.sock",
            "ftp://127.0.0.1",
            "127.0.0.1:4180",
            "http://127.0.0.1:4180/_orrery",
            "http://127.0.0.1:4180/?x=1",
            "",
        ] {
            assert!(parse_origin(raw).is_err(), "{raw:?} was accepted");
        }
    }
}
