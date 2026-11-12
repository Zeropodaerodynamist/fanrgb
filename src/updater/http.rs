//! HTTPS GET for the unelevated update helper: WinHTTP (the OS TLS stack, Schannel with the Windows
//! root store, system proxy settings), no new TLS dependency.
//!
//! [`Http`] is the seam the end-to-end tests replace with a fixture server. [`get_following`] applies
//! the redirect policy on top of it: only `https`, only GitHub's release-asset hosts, at most three hops,
//! and the `Authorization` header is never sent to a redirect target (GitHub answers an asset download
//! with a 302 to a pre-signed URL on another host).

use anyhow::Result;

use crate::error::{ErrorCode, err};

/// What came back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// `Location` header of a redirect.
    pub location: Option<String>,
    pub body: Vec<u8>,
}

pub trait Http {
    /// One GET without following redirects. Bodies larger than `max_body` are an error.
    fn get(&self, url: &str, headers: &[(&str, &str)], max_body: u64) -> Result<Response>;
}

/// The API host. Requests carrying the token only ever go here.
pub const API_HOST: &str = "api.github.com";
/// Where GitHub redirects release-asset downloads (pre-signed, no token needed).
const ASSET_HOSTS: [&str; 3] =
    ["release-assets.githubusercontent.com", "objects.githubusercontent.com", "github-releases.githubusercontent.com"];
const MAX_REDIRECTS: usize = 3;

/// `(host, path-and-query)` of a strict `https://host/path` URL: no user info, no port other than 443,
/// host made of `[a-z0-9.-]`.
pub fn split_https(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.strip_suffix(":443").unwrap_or(authority).to_ascii_lowercase();
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !host.starts_with('.')
        && !host.ends_with('.');
    let path_ok = path.bytes().all(|b| (0x21..0x7f).contains(&b));
    (host_ok && path_ok).then(|| (host, path.to_string()))
}

/// Text that may hold the token (`Bearer …`, the request header block): built in one allocation of the
/// exact size, so no growth leaves a stale copy behind, and zeroed on drop.
struct Secret(String);

impl Secret {
    fn concat(parts: &[&str]) -> Secret {
        let mut s = String::with_capacity(parts.iter().map(|p| p.len()).sum());
        parts.iter().for_each(|p| s.push_str(p));
        Secret(s)
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn wipe(&mut self) {
        // SAFETY: all-zero bytes are valid UTF-8.
        unsafe { self.0.as_bytes_mut() }.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// `Name: value\r\n` lines for `WinHttpSendRequest`, as a [`Secret`]. Names with `:` or line breaks and
/// values with line breaks are refused (header injection).
fn header_block(headers: &[(&str, &str)]) -> Result<Secret> {
    let mut parts = Vec::with_capacity(headers.len() * 4);
    for (k, v) in headers {
        if k.contains(['\r', '\n', ':']) || v.contains(['\r', '\n']) {
            return Err(err(ErrorCode::UpdateNetwork, "invalid header"));
        }
        parts.extend_from_slice(&[*k, ": ", *v, "\r\n"]);
    }
    Ok(Secret::concat(&parts))
}

/// GET with the update redirect policy. `auth` (the token) is sent to [`API_HOST`] only.
pub fn get_following(http: &dyn Http, url: &str, accept: &str, auth: Option<&str>, max_body: u64) -> Result<Response> {
    let mut url = url.to_string();
    let bearer = auth.map(|t| Secret::concat(&["Bearer ", t]));
    for hop in 0..=MAX_REDIRECTS {
        let (host, _) = split_https(&url)
            .ok_or_else(|| err(ErrorCode::UpdateNetwork, format!("refusing URL {}", redact_url(&url))))?;
        let mut headers: Vec<(&str, &str)> = vec![("Accept", accept), ("X-GitHub-Api-Version", "2022-11-28")];
        if hop == 0
            && host == API_HOST
            && let Some(b) = &bearer
        {
            headers.push(("Authorization", b.as_str()));
        }
        let r = http.get(&url, &headers, max_body)?;
        if !matches!(r.status, 301 | 302 | 303 | 307 | 308) {
            return Ok(r);
        }
        let next = r.location.ok_or_else(|| err(ErrorCode::UpdateNetwork, "redirect without a Location"))?;
        match split_https(&next) {
            Some((h, _)) if ASSET_HOSTS.contains(&h.as_str()) => url = next,
            _ => {
                return Err(err(ErrorCode::UpdateNetwork, format!("refusing redirect to {}", redact_url(&next))));
            }
        }
    }
    Err(err(ErrorCode::UpdateNetwork, "too many redirects"))
}

/// Scheme + host + path of a URL (drops the query, which holds the pre-signed credentials).
pub fn redact_url(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or("").chars().take(200).collect()
}

// ---------------------------------------------------------------------------------------
// WinHTTP

pub struct WinHttp {
    session: Handle,
}

struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a WinHTTP handle we opened and close exactly once.
            unsafe { windows_sys::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> anyhow::Error {
    err(ErrorCode::UpdateNetwork, format!("{what}: {}", std::io::Error::last_os_error()))
}

/// Sets a 4-byte WinHTTP option and fails closed: if the "never redirect" policy
/// did not take, WinHTTP would follow https redirects itself, past [`get_following`]'s host allow-list
/// and possibly with the `Authorization` header.
fn set_option_u32(h: *mut core::ffi::c_void, option: u32, value: u32, what: &str) -> Result<()> {
    // SAFETY: `h` is a WinHTTP handle (or null, which the call rejects); a 4-byte option value of the
    // documented type, alive for the call.
    let ok = unsafe {
        windows_sys::Win32::Networking::WinHttp::WinHttpSetOption(h, option, (&value as *const u32).cast(), 4)
    };
    if ok == 0 { Err(last_error(what)) } else { Ok(()) }
}

impl WinHttp {
    pub fn new() -> Result<WinHttp> {
        use windows_sys::Win32::Networking::WinHttp::*;
        let agent = crate::util::win::wide(&format!("FanRGB-Updater/{}", env!("CARGO_PKG_VERSION")));
        // SAFETY: `agent` is NUL-terminated; null proxy strings with the automatic-proxy access type.
        let s = unsafe {
            WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0)
        };
        if s.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        let session = Handle(s);
        let protocols: u32 = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
        // Fail closed, like the redirect policy: if neither protocol set takes, the session would
        // silently keep WinHTTP's defaults (which may include TLS 1.0/1.1 on older systems).
        if set_option_u32(session.0, WINHTTP_OPTION_SECURE_PROTOCOLS, protocols, "TLS 1.2/1.3").is_err() {
            // TLS 1.3 needs Windows 11 / Server 2022; fall back to TLS 1.2 only.
            set_option_u32(session.0, WINHTTP_OPTION_SECURE_PROTOCOLS, WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2, "TLS 1.2")?;
        }
        // SAFETY: a valid session handle; plain integer arguments.
        if unsafe { WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 60_000) } == 0 {
            return Err(last_error("WinHttpSetTimeouts"));
        }
        Ok(WinHttp { session })
    }
}

impl Http for WinHttp {
    fn get(&self, url: &str, headers: &[(&str, &str)], max_body: u64) -> Result<Response> {
        use windows_sys::Win32::Networking::WinHttp::*;
        let (host, path) = split_https(url)
            .ok_or_else(|| err(ErrorCode::UpdateNetwork, format!("refusing URL {}", redact_url(url))))?;
        let whost = crate::util::win::wide(&host);
        // SAFETY: valid session handle; `whost` is NUL-terminated.
        let conn = Handle(unsafe { WinHttpConnect(self.session.0, whost.as_ptr(), 443, 0) });
        if conn.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let verb = crate::util::win::wide("GET");
        let wpath = crate::util::win::wide(&path);
        // SAFETY: valid connection handle; NUL-terminated strings; null version/referrer/accept types.
        let req = Handle(unsafe {
            WinHttpOpenRequest(
                conn.0,
                verb.as_ptr(),
                wpath.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        });
        if req.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        set_option_u32(req.0, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER, "redirect policy")?;
        set_option_u32(
            req.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION,
            "disabling cookies and automatic authentication",
        )?;
        let hdr = header_block(headers)?;
        // Exact capacity (UTF-16 never needs more units than UTF-8 bytes): no growth, no stale copy.
        let mut whdr: Vec<u16> = Vec::with_capacity(hdr.as_str().len());
        whdr.extend(hdr.as_str().encode_utf16());
        // SAFETY: valid request handle; `whdr` holds exactly the length passed; no request body.
        let sent = unsafe {
            WinHttpSendRequest(req.0, whdr.as_ptr(), whdr.len() as u32, std::ptr::null(), 0, 0, 0) != 0
                && WinHttpReceiveResponse(req.0, std::ptr::null_mut()) != 0
        };
        // The header block may hold the token: wipe our copies right away (`hdr` on drop).
        whdr.iter_mut().for_each(|w| *w = 0);
        std::hint::black_box(&whdr);
        drop(hdr);
        if !sent {
            return Err(last_error(&format!("GET https://{host}")));
        }
        let mut status: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: valid request handle; `status` is a 4-byte buffer and `len` its size.
        let ok = unsafe {
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut status as *mut u32 as *mut _,
                &mut len,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error("status code"));
        }
        let mut location = None;
        if (300..400).contains(&status) {
            let mut buf = vec![0u16; 4096];
            let mut blen = (buf.len() * 2) as u32;
            // SAFETY: valid request handle; `buf` is writable for `blen` bytes.
            if unsafe {
                WinHttpQueryHeaders(
                    req.0,
                    WINHTTP_QUERY_LOCATION,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut _,
                    &mut blen,
                    std::ptr::null_mut(),
                )
            } != 0
            {
                location = Some(String::from_utf16_lossy(&buf[..blen as usize / 2]));
            }
            return Ok(Response { status: status as u16, location, body: Vec::new() });
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut got: u32 = 0;
            // SAFETY: valid request handle; `chunk` is writable for its length.
            if unsafe { WinHttpReadData(req.0, chunk.as_mut_ptr() as *mut _, chunk.len() as u32, &mut got) } == 0 {
                return Err(last_error("reading the response"));
            }
            if got == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..got as usize]);
            if body.len() as u64 > max_body {
                return Err(err(
                    ErrorCode::UpdateNetwork,
                    format!("response from {host} larger than {max_body} bytes"),
                ));
            }
        }
        Ok(Response { status: status as u16, location, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn urls_are_strict() {
        assert_eq!(
            split_https("https://api.github.com/repositories/1/releases/latest"),
            Some(("api.github.com".into(), "/repositories/1/releases/latest".into()))
        );
        assert_eq!(split_https("https://API.github.com:443/x").unwrap().0, "api.github.com");
        for bad in [
            "http://api.github.com/x",
            "https://user@api.github.com/x",
            "https://api.github.com:8443/x",
            "https://api.github.com/a b",
            "https://.evil/x",
            "ftp://x/y",
            "https:///x",
        ] {
            assert_eq!(split_https(bad), None, "{bad}");
        }
        assert_eq!(redact_url("https://h/p?X-Amz-Signature=abc#f"), "https://h/p");
    }

    type Headers = Vec<(String, String)>;

    /// Records every request; answers from a list.
    struct Script {
        answers: RefCell<Vec<Response>>,
        seen: RefCell<Vec<(String, Headers)>>,
    }

    impl Http for Script {
        fn get(&self, url: &str, headers: &[(&str, &str)], _max: u64) -> Result<Response> {
            self.seen
                .borrow_mut()
                .push((url.into(), headers.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()));
            Ok(self.answers.borrow_mut().remove(0))
        }
    }

    fn redirect(to: &str) -> Response {
        Response { status: 302, location: Some(to.into()), body: vec![] }
    }

    #[test]
    fn token_never_follows_a_redirect() {
        let s = Script {
            answers: RefCell::new(vec![
                redirect("https://release-assets.githubusercontent.com/a/b?sig=1"),
                Response { status: 200, location: None, body: b"bytes".to_vec() },
            ]),
            seen: RefCell::new(vec![]),
        };
        let r = get_following(
            &s,
            "https://api.github.com/repositories/1/releases/assets/2",
            "application/octet-stream",
            Some("github_pat_x"),
            10,
        )
        .unwrap();
        assert_eq!(r.body, b"bytes");
        let seen = s.seen.borrow();
        assert!(seen[0].1.iter().any(|(k, v)| k == "Authorization" && v == "Bearer github_pat_x"));
        assert!(seen[1].1.iter().all(|(k, _)| k != "Authorization"), "token sent to the redirect target");
    }

    /// Code review v0.2 L16: a WinHTTP option that does not take fails the request (E702), it is not
    /// ignored.
    #[test]
    fn failed_option_fails_closed() {
        use windows_sys::Win32::Networking::WinHttp::{
            WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        };
        let e = set_option_u32(
            std::ptr::null_mut(),
            WINHTTP_OPTION_REDIRECT_POLICY,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
            "redirect policy",
        )
        .unwrap_err();
        assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateNetwork);
    }

    /// Code review v0.2 L17: the bearer value and the header block are exact-size (no stale copy from
    /// growth) and wiped.
    #[test]
    fn token_buffers_are_exact_and_wiped() {
        let mut b = Secret::concat(&["Bearer ", "github_pat_abc"]);
        assert_eq!(b.as_str(), "Bearer github_pat_abc");
        assert_eq!(b.0.capacity(), b.0.len());
        b.wipe();
        assert!(b.0.bytes().all(|x| x == 0) && b.0.len() == 21);
        let h = header_block(&[("Accept", "*/*"), ("Authorization", "Bearer t")]).unwrap();
        assert_eq!(h.as_str(), "Accept: */*\r\nAuthorization: Bearer t\r\n");
        assert_eq!(h.0.capacity(), h.0.len());
        assert!(header_block(&[("A", "x\r\nInjected: 1")]).is_err());
        assert!(header_block(&[("A:B", "x")]).is_err());
    }

    #[test]
    fn redirects_only_to_github_asset_hosts() {
        for target in
            ["https://evil.example/x", "http://objects.githubusercontent.com/x", "https://api.github.com/other"]
        {
            let s = Script { answers: RefCell::new(vec![redirect(target)]), seen: RefCell::new(vec![]) };
            let e = get_following(&s, "https://api.github.com/x", "*/*", Some("t"), 10).unwrap_err();
            assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateNetwork, "{target}");
        }
        // The token is not sent to a non-API first URL either.
        let s = Script {
            answers: RefCell::new(vec![Response { status: 200, ..Default::default() }]),
            seen: RefCell::new(vec![]),
        };
        get_following(&s, "https://objects.githubusercontent.com/x", "*/*", Some("t"), 10).unwrap();
        assert!(s.seen.borrow()[0].1.iter().all(|(k, _)| k != "Authorization"));
        // Redirect loops end.
        let s = Script {
            answers: RefCell::new((0..5).map(|_| redirect("https://objects.githubusercontent.com/x")).collect()),
            seen: RefCell::new(vec![]),
        };
        assert!(get_following(&s, "https://api.github.com/x", "*/*", None, 10).is_err());
    }

    // Independent tester (2026-11-06): the token rules with and without a token.

    /// Through a two-hop redirect chain: without a token no hop has an `Authorization` header; with one,
    /// only the first request (to the API host) has it.
    #[test]
    fn tester_authorization_only_on_the_first_api_hop() {
        let chain = || {
            vec![
                redirect("https://release-assets.githubusercontent.com/a?sig=1"),
                redirect("https://objects.githubusercontent.com/b?sig=2"),
                Response { status: 200, location: None, body: b"ok".to_vec() },
            ]
        };
        for auth in [None, Some("github_pat_x")] {
            let s = Script { answers: RefCell::new(chain()), seen: RefCell::new(vec![]) };
            let url = "https://api.github.com/repositories/1/releases/assets/2";
            let r = get_following(&s, url, "application/octet-stream", auth, 10).unwrap();
            assert_eq!(r.body, b"ok");
            let seen = s.seen.borrow();
            assert_eq!(seen.len(), 3);
            for (i, (url, headers)) in seen.iter().enumerate() {
                let n = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).count();
                assert_eq!(n, usize::from(auth.is_some() && i == 0), "{auth:?}: {url}");
                assert!(headers.iter().all(|(_, v)| !v.contains("github_pat_x") || i == 0), "{url}");
            }
        }
    }

    /// Hosts that only look like the API host never receive the token; the API host in capitals is the
    /// API host.
    #[test]
    fn tester_token_only_for_the_real_api_host() {
        for url in [
            "https://api.github.com.evil.example/x",
            "https://evil.example/api.github.com/x",
            "https://xapi.github.com/x",
            "https://github.com/x",
            "https://uploads.github.com/x",
            "https://release-assets.githubusercontent.com/x",
        ] {
            let s = Script {
                answers: RefCell::new(vec![Response { status: 200, ..Default::default() }]),
                seen: RefCell::new(vec![]),
            };
            get_following(&s, url, "*/*", Some("github_pat_x"), 10).unwrap();
            assert!(s.seen.borrow()[0].1.iter().all(|(k, _)| k != "Authorization"), "token sent to {url}");
        }
        let s = Script {
            answers: RefCell::new(vec![Response { status: 200, ..Default::default() }]),
            seen: RefCell::new(vec![]),
        };
        get_following(&s, "https://API.GitHub.com:443/x", "*/*", Some("github_pat_x"), 10).unwrap();
        assert!(s.seen.borrow()[0].1.iter().any(|(k, _)| k == "Authorization"));
    }

    /// Without a token the redirect policy is the same: off the asset hosts, or plain http, is refused
    /// before a second request is made.
    #[test]
    fn tester_redirect_policy_is_the_same_without_a_token() {
        for target in [
            "https://evil.example/x",
            "http://objects.githubusercontent.com/x",
            "https://api.github.com/repositories/2/releases/assets/3",
            "https://objects.githubusercontent.com.evil.example/x",
            "//objects.githubusercontent.com/x",
        ] {
            let s = Script { answers: RefCell::new(vec![redirect(target)]), seen: RefCell::new(vec![]) };
            let e = get_following(&s, "https://api.github.com/x", "*/*", None, 10).unwrap_err();
            assert_eq!(crate::error::code_of(&e), ErrorCode::UpdateNetwork, "{target}");
            assert_eq!(s.seen.borrow().len(), 1, "{target} was requested");
        }
    }
}
