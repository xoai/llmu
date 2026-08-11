use anyhow::Result;
use std::fmt;
use std::time::Duration;

/// A non-2xx HTTP status returned by an endpoint. `code` carries the
/// status; `note` is the one-line body snippet the caller saw.
#[derive(Debug)]
pub struct HttpStatusError {
    pub code: u16,
    pub note: String,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}: {}", self.code, self.note)
    }
}

impl std::error::Error for HttpStatusError {}

/// Classify a failed cost-report fetch for the user-visible notes.
/// A 404 means the endpoint is absent (some org types lack it) and is a
/// silent break; every other failure gets a note so the user knows the
/// billed totals may be incomplete.
pub fn cost_break_note(e: &anyhow::Error, provider: &str) -> Option<String> {
    match e.downcast_ref::<HttpStatusError>() {
        Some(se) if se.code == 404 => None,
        _ => Some(format!(
            "{provider}: cost report failed ({e}) — billed totals may be incomplete"
        )),
    }
}

/// Set LLMU_DEBUG=1 to dump every request URL and a response snippet to
/// stderr — the fast way to diagnose payload drift on the undocumented
/// endpoints (they can change shape without notice).
fn debug() -> bool {
    std::env::var("LLMU_DEBUG")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

pub fn get_json(url: &str, headers: &[(&str, &str)]) -> Result<serde_json::Value> {
    let mut req = ureq::get(url).timeout(Duration::from_secs(30));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(r) => {
            let body = r.into_string()?;
            if debug() {
                let snip: String = body.chars().take(800).collect();
                eprintln!("[debug] GET {url}\n[debug] 200: {snip}");
            }
            Ok(serde_json::from_str(&body)?)
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            if debug() {
                let snip: String = body.chars().take(800).collect();
                eprintln!("[debug] GET {url}\n[debug] {code}: {snip}");
            }
            // Notes stay one line: collapse whitespace, cap length.
            let compact: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet: String = compact.chars().take(200).collect();
            Err(anyhow::Error::new(HttpStatusError {
                code,
                note: snippet,
            }))
        }
        Err(e) => {
            if debug() {
                eprintln!("[debug] GET {url}\n[debug] transport error: {e}");
            }
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_err(code: u16) -> anyhow::Error {
        anyhow::Error::new(HttpStatusError {
            code,
            note: "no note needed".into(),
        })
    }

    #[test]
    fn cost_break_note_404_is_none() {
        assert!(cost_break_note(&status_err(404), "anthropic").is_none());
    }

    #[test]
    fn cost_break_note_non_404_contains_provider_and_code() {
        let n = cost_break_note(&status_err(403), "anthropic").expect("403 must yield a note");
        assert!(n.contains("anthropic"));
        assert!(n.contains("403"));
        assert!(n.contains("billed totals may be incomplete"));
    }

    #[test]
    fn cost_break_note_transport_error_is_some() {
        let n = cost_break_note(&anyhow::anyhow!("connection refused"), "openai")
            .expect("transport error must yield a note");
        assert!(n.contains("openai"));
    }
}
