//! Keeps the server private. It only listens on this computer, but any website open in a browser
//! here could still send it requests, so each request has to pass these checks:
//!
//! - The `Host` must be this server's own address, which stops a website pointing its own name
//!   at this computer to get around the browser's rules.
//! - A request a website started is refused, going by `Origin` and `Sec-Fetch-Site`. One that
//!   changes something must state its `Origin`, and be JSON, which no plain form can send.
//! - It must carry the secret cookie, which comes from opening the link printed at the start.
//!   The link works once, as it passes through other programs' command lines on the way to the
//!   browser. The cookie doesn't, and page scripts can't read it.

use std::fs::File;
use std::io::{self, Read};
use std::sync::Mutex;

/// The request headers that matter here.
#[derive(Debug, Default)]
pub struct Headers {
    pub host: Option<String>,
    pub origin: Option<String>,
    pub fetch_site: Option<String>,
    pub cookie: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// The link was just used. Holds the `Set-Cookie` value.
    LoggedIn(String),
    /// Already signed in, so take the secret out of the address bar.
    DropToken,
    Refuse(u16, &'static str),
}

const EXPIRED: &str = "This page needs the link ccm web printed when it started. A link works once, so start ccm web again to get a new one.";

pub struct Guard {
    host: String,
    origin: String,
    /// Named after the port, as cookies are shared between the ports of one address.
    cookie_name: String,
    session: String,
    login: Mutex<Option<String>>,
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Takes as long wherever the two differ, so the time taken gives nothing away.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Guard {
    /// The guard and the secret for the one-time link.
    pub fn new(port: u16) -> io::Result<(Self, String)> {
        let login = random_hex()?;
        let guard = Self {
            host: format!("127.0.0.1:{port}"),
            origin: format!("http://127.0.0.1:{port}"),
            cookie_name: format!("ccm_{port}"),
            session: random_hex()?,
            login: Mutex::new(Some(login.clone())),
        };
        Ok((guard, login))
    }

    fn has_session(&self, headers: &Headers) -> bool {
        let Some(cookies) = &headers.cookie else { return false };
        cookies.split(';').filter_map(|c| c.trim().split_once('=')).any(|(name, value)| name == self.cookie_name && same(value, &self.session))
    }

    fn set_cookie(&self) -> String {
        format!("{}={}; Path=/; HttpOnly; SameSite=Strict", self.cookie_name, self.session)
    }

    /// `token` is the one from the address, if any.
    pub fn check(&self, headers: &Headers, method: &str, path: &str, token: Option<&str>) -> Verdict {
        if headers.host.as_deref() != Some(self.host.as_str()) {
            return Verdict::Refuse(403, "Open ccm web using the link it printed.");
        }
        if headers.origin.as_deref().is_some_and(|o| o != self.origin) {
            return Verdict::Refuse(403, "Requests from other websites aren't accepted.");
        }
        if headers.fetch_site.as_deref().is_some_and(|s| s != "same-origin" && s != "none") {
            return Verdict::Refuse(403, "Requests from other websites aren't accepted.");
        }
        if method != "GET" {
            let json = headers.content_type.as_deref().is_some_and(|t| t.split(';').next().is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json")));
            if headers.origin.is_none() || !json {
                return Verdict::Refuse(403, "Requests from other websites aren't accepted.");
            }
        }
        let signed_in = self.has_session(headers);
        if path == "/"
            && let Some(token) = token
        {
            if signed_in {
                return Verdict::DropToken;
            }
            let mut login = self.login.lock().unwrap_or_else(|e| e.into_inner());
            if login.as_deref().is_some_and(|l| same(l, token)) {
                *login = None;
                return Verdict::LoggedIn(self.set_cookie());
            }
            return Verdict::Refuse(401, EXPIRED);
        }
        if signed_in { Verdict::Pass } else { Verdict::Refuse(401, EXPIRED) }
    }
}

#[cfg(test)]
pub fn cookie_pair(guard: &Guard) -> String {
    format!("{}={}", guard.cookie_name, guard.session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> (Guard, String) {
        Guard::new(4000).unwrap()
    }

    fn signed_in(g: &Guard) -> Headers {
        Headers { host: Some("127.0.0.1:4000".into()), cookie: Some(cookie_pair(g)), ..Headers::default() }
    }

    #[test]
    fn a_cookie_lets_a_request_through() {
        let (g, _) = guard();
        assert_eq!(g.check(&signed_in(&g), "GET", "/api/list", None), Verdict::Pass);
    }

    #[test]
    fn no_or_a_wrong_cookie_is_refused() {
        let (g, _) = guard();
        let mut h = signed_in(&g);
        h.cookie = None;
        assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(401, _)));
        h.cookie = Some("ccm_4000=wrong".into());
        assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(401, _)));
        h.cookie = Some(cookie_pair(&g).replace("ccm_4000", "ccm_4001"));
        assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(401, _)), "another server's cookie");
    }

    #[test]
    fn the_cookie_is_found_among_others() {
        let (g, _) = guard();
        let mut h = signed_in(&g);
        h.cookie = Some(format!("a=b; {}; c=d", cookie_pair(&g)));
        assert_eq!(g.check(&h, "GET", "/", None), Verdict::Pass);
    }

    #[test]
    fn a_name_pointed_at_this_computer_is_refused_even_signed_in() {
        let (g, _) = guard();
        for host in [Some("evil.example:4000"), Some("localhost:4000"), Some("127.0.0.1"), Some("127.0.0.1:4001"), None] {
            let mut h = signed_in(&g);
            h.host = host.map(str::to_string);
            assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(403, _)), "{host:?}");
        }
    }

    #[test]
    fn requests_from_other_websites_are_refused_even_signed_in() {
        let (g, _) = guard();
        let mut h = signed_in(&g);
        h.origin = Some("https://evil.example".into());
        assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(403, _)));
        h.origin = Some("null".into());
        assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(403, _)), "a sandboxed frame");
        let mut h = signed_in(&g);
        for site in ["cross-site", "same-site"] {
            h.fetch_site = Some(site.into());
            assert!(matches!(g.check(&h, "GET", "/api/list", None), Verdict::Refuse(403, _)), "{site}");
        }
    }

    #[test]
    fn the_page_itself_is_accepted() {
        let (g, _) = guard();
        let mut h = signed_in(&g);
        h.origin = Some("http://127.0.0.1:4000".into());
        h.fetch_site = Some("same-origin".into());
        assert_eq!(g.check(&h, "GET", "/api/list", None), Verdict::Pass);
        h.fetch_site = Some("none".into());
        assert_eq!(g.check(&h, "GET", "/", None), Verdict::Pass);
    }

    #[test]
    fn a_change_needs_its_origin_stated_and_json() {
        let (g, _) = guard();
        let origin = || Some("http://127.0.0.1:4000".to_string());
        let json = || Some("application/json".to_string());
        let check = |origin: Option<String>, content_type: Option<String>| {
            let h = Headers { origin, content_type, ..signed_in(&g) };
            g.check(&h, "POST", "/api/rename", None)
        };
        assert_eq!(check(origin(), json()), Verdict::Pass);
        assert_eq!(check(origin(), Some("Application/JSON; charset=utf-8".into())), Verdict::Pass);
        assert!(matches!(check(None, json()), Verdict::Refuse(403, _)), "no origin");
        assert!(matches!(check(origin(), None), Verdict::Refuse(403, _)), "no type");
        for form in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data"] {
            assert!(matches!(check(origin(), Some(form.into())), Verdict::Refuse(403, _)), "{form}");
        }
    }

    #[test]
    fn a_change_from_another_website_is_refused() {
        let (g, _) = guard();
        let h = Headers { origin: Some("http://127.0.0.1:9999".into()), content_type: Some("application/json".into()), ..signed_in(&g) };
        assert!(matches!(g.check(&h, "POST", "/api/rename", None), Verdict::Refuse(403, _)));
    }

    #[test]
    fn a_change_without_the_cookie_is_refused() {
        let (g, _) = guard();
        let h = Headers { host: Some("127.0.0.1:4000".into()), origin: Some("http://127.0.0.1:4000".into()), content_type: Some("application/json".into()), ..Headers::default() };
        assert!(matches!(g.check(&h, "POST", "/api/rename", None), Verdict::Refuse(401, _)));
    }

    #[test]
    fn the_link_works_once() {
        let (g, login) = guard();
        let h = Headers { host: Some("127.0.0.1:4000".into()), fetch_site: Some("none".into()), ..Headers::default() };
        let Verdict::LoggedIn(cookie) = g.check(&h, "GET", "/", Some(&login)) else { panic!("should sign in") };
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict") && cookie.contains(&cookie_pair(&g)));
        assert!(matches!(g.check(&h, "GET", "/", Some(&login)), Verdict::Refuse(401, _)));
    }

    #[test]
    fn a_wrong_token_does_not_use_up_the_link() {
        let (g, login) = guard();
        let h = Headers { host: Some("127.0.0.1:4000".into()), ..Headers::default() };
        assert!(matches!(g.check(&h, "GET", "/", Some("wrong")), Verdict::Refuse(401, _)));
        assert!(matches!(g.check(&h, "GET", "/", Some(&login)), Verdict::LoggedIn(_)));
    }

    #[test]
    fn the_link_only_works_on_the_home_page() {
        let (g, login) = guard();
        let h = Headers { host: Some("127.0.0.1:4000".into()), ..Headers::default() };
        assert!(matches!(g.check(&h, "GET", "/api/list", Some(&login)), Verdict::Refuse(401, _)));
        assert!(matches!(g.check(&h, "GET", "/", Some(&login)), Verdict::LoggedIn(_)));
    }

    #[test]
    fn opening_the_link_again_in_the_same_browser_just_goes_home() {
        let (g, login) = guard();
        assert_eq!(g.check(&signed_in(&g), "GET", "/", Some(&login)), Verdict::DropToken);
    }

    #[test]
    fn secrets_differ_and_are_long() {
        let (a, a_login) = guard();
        let (b, _) = guard();
        assert_ne!(a.session, b.session);
        assert_ne!(a.session, a_login);
        assert_eq!(a_login.len(), 64);
    }
}
