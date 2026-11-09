# Shared behavior

What every route of the GitHub face shares. It is built once for the engine, not per endpoint (rule 1), because a mock that gets it wrong breaks clients in ways no single endpoint's test catches.

## Where the face is

Orrery presents itself as a GitHub Enterprise Server at its `base_url`, so a client that takes a GitHub Enterprise base URL needs nothing else:

| Surface | Path |
| --- | --- |
| REST | `{base_url}/api/v3/…` |
| GraphQL | `POST {base_url}/api/graphql`, a sibling of `/api/v3`, never under it |
| git | `{base_url}/{owner}/{repo}[.git]/…` (`surface-git.md`) |
| OAuth web flow | `{base_url}/login/oauth/…` |

github.com's `api.github.com` and the data-residency `api.<tenant>.ghe.com` host shapes are not served. The control plane is under `/_orrery`, on the same listener unless `server.control_listen` moves it; a faces listener then answers `/_orrery/…` as surface it does not serve.

## Authentication

REST, GraphQL, and git need a token, in any of the spellings GitHub's clients send:

- `Authorization: Bearer <token>` or `Authorization: token <token>` (the scheme ignoring case), or
- `Authorization: Basic base64(<anything>:<token>)`, git's `x-access-token:<token>` convention.

A missing, malformed, or unknown token is `401` `{"message": "Bad credentials"}` with `WWW-Authenticate: Basic realm="orrery"`. The one exception is anonymous git fetch, described in `surface-git.md`. The OAuth web flow authenticates with client credentials and a login instead (`surface-github.md` §2).

Every token is fake, prefixed `orrfake_`, and derived from the world's secret, so a world holds no real credential (rule 12). Tokens carry no scopes, and no route checks permissions: a token is only an actor's identity.

## Requests

- **Bodies decode laxly, as GitHub's do.** An unknown field is ignored. A known field Orrery does not implement is refused by its presence, whatever its value, with `422`:

  ```json
  {"message": "orrery: request field \"F\" is not mocked",
   "orrery": {"not_mocked": true, "field": "F", "hint": "…"},
   "errors": [{"resource": "…", "code": "custom", "field": "F", "message": "not mocked by orrery"}]}
  ```

  `resource` names what the route is about: `Repository`, `PullRequest`, `PullRequestReview`, `Label`, `CheckRun`, `Team`, `Comparison`, or `OauthAccess` for the OAuth token exchange. A media type a route lists as not mocked (the diff and patch media types, and XML from the OAuth token exchange) is refused the same way, with `field` `Accept`.

- A body that isn't valid JSON is `400` `{"message": "Problems parsing JSON"}`, and on most routes so is a known field of the wrong type. A body over 2 MiB is `413`.
- Query values are decoded as a URL query is: `%XX`, and `+` as a space. A repeated parameter keeps its first value.
- Logins, organization names, and repository names match ignoring case; responses carry each one's stored spelling. A `{number}` that isn't an integer is `404`.

## Responses

- JSON, `application/json; charset=utf-8`. Times are RFC 3339 in UTC, read from the world clock.
- Node ids are stable strings that REST's `node_id` and GraphQL's `id` share: `O_actor_N`, `O_repo_N`, `O_pull_N`, `O_review_N`, `O_comment_N`, `O_check_N`, and `O_team_N`.
- GitHub's error bodies: `{"message"}` for `404` "Not Found" and the like, and for a validation failure `422` `{"message": "Validation Failed", "errors": [{"resource", "code", "field"}]}`, where `code` is `missing_field`, `invalid`, `already_exists`, or `custom` (with a `message` and no `field`). Repository creation uses GitHub's own envelope, "Repository creation failed.". A route reports the first failing field.

## Surface Orrery doesn't serve

A REST path or method with no route answers `501`, deliberately unlike any GitHub response, so it can't be read as "no such thing". It names the method and the path as the request sent it:

```json
{"message": "orrery: GET /api/v3/rate_limit is not mocked",
 "orrery": {"not_mocked": true, "method": "GET", "path": "/api/v3/rate_limit",
            "hint": "If a consumer needs this endpoint, add it to the surface inventory (spec/surface-github.md) and implement it."}}
```

GraphQL marks the same fault in its `errors` entry (`surface-github.md` §3.1). A matched route with no data answers GitHub's own `404`; the two never blur.

## Pagination

REST lists page with `?per_page=&page=`: `per_page` defaults to 30 and is limited to 1 through 100, as on GitHub, and `page` starts at 1. There is no `Link` header and no total, so a client pages until a short page. Every list has a fixed total order, so a page boundary never drops or repeats an item. GraphQL connections are in `surface-github.md` §3.4.

## Conditional requests

`GET …/pulls` (every page) and `GET …/git/ref/heads/{branch}` carry a weak `ETag` derived from the exact body. An `If-None-Match` equal to it answers `304` with no body and the same `ETag`, so the tag changes exactly when the answer does. No other route answers `304`.

## Rate limits

Orrery doesn't rate limit. It sends no `X-RateLimit-*` headers, GraphQL `rateLimit` is `null`, and `/rate_limit` is not served: the shape of a GitHub Enterprise Server with rate limiting turned off, which GitHub's clients handle.

## Known gaps

- A path Orrery routes, called with a method it doesn't serve (`GET /api/graphql`, `PATCH /repos/{o}/{r}`), answers a bare `405` rather than the `501` above.
- A GitHub media type Orrery doesn't render, other than the diff and patch media types the routes that list them refuse, gets the plain JSON answer: `application/vnd.github.full+json` on a pull request has no `body_html`, and `application/vnd.github.raw+json` on contents answers the JSON object, not the file.
