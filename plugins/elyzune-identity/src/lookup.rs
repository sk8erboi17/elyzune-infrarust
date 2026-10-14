//! Who owns a name: Mojang's profile lookup, rate-limited and cached.
//!
//! Every answer that is not a clear "a paid account owns it" or "nobody
//! does" is an error, and the caller refuses the login: a name whose owner
//! cannot be established must not slip into the password route, where its
//! paid owner would find a stranger's character under their name.
//!
//! A demo profile (`"demo": true`) owns no game, so it does not count as a
//! paid account.

use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::InMemoryState, state::NotKeyed};

use crate::config::{CacheConfig, RateConfig};

/// Who owns a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// A paid Mojang account; `name` is its own spelling.
    Paid { name: String },
    /// Nobody (or a demo account).
    Free,
}

/// Why a name could not be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// Our own budget of lookups is spent, or Mojang answered 429.
    RateLimited,
    /// No answer in time, a network failure, or an answer we cannot read.
    Unavailable(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => f.write_str("rate limited"),
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
        }
    }
}

#[derive(serde::Deserialize)]
struct Profile {
    name: String,
    #[serde(default)]
    demo: bool,
}

/// The HTTP side.
pub struct MojangLookup {
    client: reqwest::Client,
    url: String,
    limiter: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

impl MojangLookup {
    /// # Errors
    /// When the HTTP client cannot be built.
    pub fn new(url: &str, timeout: Duration, rate: RateConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .user_agent(concat!("elyzune-identity/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| error.to_string())?;
        let period = Duration::from_millis(rate.refill_ms.max(1));
        let quota = Quota::with_period(period)
            .ok_or("a zero refill period")?
            .allow_burst(NonZeroU32::new(rate.burst).unwrap_or(NonZeroU32::MIN));
        Ok(Self {
            client,
            url: url.trim_end_matches('/').to_owned(),
            limiter: RateLimiter::direct(quota),
        })
    }

    /// Asks Mojang who owns `name`.
    ///
    /// # Errors
    /// [`LookupError`] for anything but a clear answer.
    pub async fn owner(&self, name: &str) -> Result<Ownership, LookupError> {
        if self.limiter.check().is_err() {
            return Err(LookupError::RateLimited);
        }
        let response = self
            .client
            .get(format!("{}/{name}", self.url))
            .send()
            .await
            .map_err(|error| LookupError::Unavailable(error.to_string()))?;
        match response.status().as_u16() {
            200 => {
                let profile: Profile = response
                    .json()
                    .await
                    .map_err(|error| LookupError::Unavailable(format!("bad profile: {error}")))?;
                if profile.demo {
                    Ok(Ownership::Free)
                } else if profile.name.eq_ignore_ascii_case(name) {
                    Ok(Ownership::Paid { name: profile.name })
                } else {
                    Err(LookupError::Unavailable(format!(
                        "asked for {name}, answered {}",
                        profile.name
                    )))
                }
            }
            204 | 404 => Ok(Ownership::Free),
            429 => Err(LookupError::RateLimited),
            status => Err(LookupError::Unavailable(format!("status {status}"))),
        }
    }
}

/// What is remembered per name (lower case): who owns it, and whether its
/// Mojang authentication just failed.
pub struct Memory {
    owners: DashMap<String, (Ownership, Instant)>,
    failed: DashMap<String, Instant>,
    config: CacheConfig,
    failed_for: Duration,
}

impl Memory {
    #[must_use]
    pub fn new(config: CacheConfig, failed_for: Duration) -> Self {
        Self {
            owners: DashMap::new(),
            failed: DashMap::new(),
            config,
            failed_for,
        }
    }

    fn ttl(&self, ownership: &Ownership) -> Duration {
        Duration::from_secs(match ownership {
            Ownership::Paid { .. } => self.config.paid_seconds,
            Ownership::Free => self.config.free_seconds,
        })
    }

    /// The remembered owner of `name`, while fresh.
    #[must_use]
    pub fn owner(&self, name: &str) -> Option<Ownership> {
        let entry = self.owners.get(&name.to_lowercase())?;
        let (ownership, at) = entry.value();
        (at.elapsed() < self.ttl(ownership)).then(|| ownership.clone())
    }

    /// Remembers who owns `name`. A full memory first forgets what expired;
    /// if it is still full the answer is simply not kept (the next login
    /// asks again), so memory never grows past `capacity`.
    pub fn remember(&self, name: &str, ownership: Ownership) {
        if self.owners.len() >= self.config.capacity {
            self.sweep();
            if self.owners.len() >= self.config.capacity {
                return;
            }
        }
        self.owners
            .insert(name.to_lowercase(), (ownership, Instant::now()));
    }

    /// A Mojang authentication of `name` failed.
    pub fn failed(&self, name: &str) {
        if self.failed.len() >= self.config.capacity {
            self.sweep();
        }
        self.failed.insert(name.to_lowercase(), Instant::now());
    }

    /// Whether a Mojang authentication of `name` failed recently.
    #[must_use]
    pub fn recently_failed(&self, name: &str) -> bool {
        self.failed
            .get(&name.to_lowercase())
            .is_some_and(|at| at.elapsed() < self.failed_for)
    }

    /// Forgets every expired entry.
    pub fn sweep(&self) {
        self.owners
            .retain(|_, (ownership, at)| at.elapsed() < self.ttl(ownership));
        let failed_for = self.failed_for;
        self.failed.retain(|_, at| at.elapsed() < failed_for);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A loopback stand-in for Mojang that answers each request with the
    /// next of `answers` (status, body).
    async fn mojang(answers: Vec<(u16, &'static str)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status, body) in answers {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut request).await;
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: \
                     {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
            }
        });
        format!("http://{address}/users/profiles/minecraft")
    }

    fn lookup(url: &str, burst: u32) -> MojangLookup {
        MojangLookup::new(
            url,
            Duration::from_millis(500),
            RateConfig {
                burst,
                refill_ms: 60_000,
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn paid_free_demo_and_failures() {
        let url = mojang(vec![
            (
                200,
                r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}"#,
            ),
            (204, ""),
            (404, ""),
            (200, r#"{"id":"0","name":"Demo","demo":true}"#),
            (429, ""),
            (500, ""),
            (200, "not json"),
        ])
        .await;
        let lookup = lookup(&url, 100);
        assert_eq!(
            lookup.owner("notch").await,
            Ok(Ownership::Paid {
                name: "Notch".to_owned()
            })
        );
        assert_eq!(lookup.owner("Nobody").await, Ok(Ownership::Free));
        assert_eq!(lookup.owner("Nobody2").await, Ok(Ownership::Free));
        assert_eq!(lookup.owner("Demo").await, Ok(Ownership::Free));
        assert_eq!(lookup.owner("Busy").await, Err(LookupError::RateLimited));
        assert!(matches!(
            lookup.owner("Down").await,
            Err(LookupError::Unavailable(_))
        ));
        assert!(matches!(
            lookup.owner("Odd").await,
            Err(LookupError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn the_budget_and_the_deadline_fail_closed() {
        let url = mojang(vec![(204, "")]).await;
        let lookup = lookup(&url, 1);
        assert_eq!(lookup.owner("First").await, Ok(Ownership::Free));
        assert_eq!(lookup.owner("Second").await, Err(LookupError::RateLimited));
        // Nobody listening: refused or timed out, never "free".
        let gone = MojangLookup::new(
            "http://127.0.0.1:9/x",
            Duration::from_millis(200),
            RateConfig {
                burst: 5,
                refill_ms: 1000,
            },
        )
        .unwrap();
        assert!(matches!(
            gone.owner("Anyone").await,
            Err(LookupError::Unavailable(_))
        ));
    }

    #[test]
    fn memory_expires_and_stays_bounded() {
        let memory = Memory::new(
            CacheConfig {
                paid_seconds: 3600,
                free_seconds: 0,
                capacity: 2,
            },
            Duration::from_secs(60),
        );
        memory.remember(
            "Notch",
            Ownership::Paid {
                name: "Notch".to_owned(),
            },
        );
        memory.remember("Steve", Ownership::Free);
        assert!(memory.owner("NOTCH").is_some());
        assert_eq!(memory.owner("steve"), None, "a free name expired at once");
        memory.remember("Alex", Ownership::Free);
        assert!(memory.owners.len() <= 2);
        assert!(!memory.recently_failed("Notch"));
        memory.failed("Notch");
        assert!(memory.recently_failed("notch"));
    }
}
