use std::{borrow::Cow, fmt, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}};

use actix_web::dev::ServiceRequest;
use deadpool_redis::{Pool};

mod builder;
mod errors;
mod middleware;
mod status;

pub use self::{builder::Builder, errors::Error, middleware::RateLimiter, status::Status};

const LUA: &str = r#"
local key   = KEYS[1]
local limit = tonumber(ARGV[1])
local win   = tonumber(ARGV[2])
local now   = tonumber(ARGV[3])

local cnt = redis.call("INCR", key)
if cnt == 1 then
    redis.call("EXPIRE", key, win)
end

local ttl = redis.call("TTL", key)
if ttl < 0 then ttl = win end

local limited = cnt > limit and 1 or 0
local remaining = limited == 1 and 0 or (limit - cnt)
return {limited, remaining, now + ttl}
"#;

/// Default request limit.
pub const DEFAULT_REQUEST_LIMIT: usize = 5000;

/// Default period (in seconds).
pub const DEFAULT_PERIOD_SECS: u64 = 3600;

/// Default cookie name.
pub const DEFAULT_COOKIE_NAME: &str = "sid";

/// Default session key.
#[cfg(feature = "session")]
pub const DEFAULT_SESSION_KEY: &str = "rate-api-id";

/// Helper trait to impl Debug on GetKeyFn type
trait GetKeyFnT: Fn(&ServiceRequest) -> Option<String> {}

impl<T> GetKeyFnT for T where T: Fn(&ServiceRequest) -> Option<String> {}

/// Get key function type with auto traits
type GetKeyFn = dyn GetKeyFnT + Send + Sync;

/// Get key resolver function type
impl fmt::Debug for GetKeyFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GetKeyFn")
    }
}

/// Wrapped Get key function Trait
type GetArcBoxKeyFn = Arc<GetKeyFn>;

/// Rate limiter.
#[derive(Debug, Clone)]
pub struct Limiter {
    pool: Arc<Pool>,
    limit: usize,
    period: Duration,
    get_key_fn: GetArcBoxKeyFn,
}

impl Limiter {
    /// Construct rate limiter builder with defaults.
    ///
    /// See [`redis-rs` docs](https://docs.rs/redis/0.21/redis/#connection-parameters) on connection
    /// parameters for how to set the Redis URL.
    #[must_use]
    pub fn builder(r: Arc<Pool>) -> Builder {
        Builder {
            redis: r,
            limit: DEFAULT_REQUEST_LIMIT,
            period: Duration::from_secs(DEFAULT_PERIOD_SECS),
            get_key_fn: None,
            cookie_name: Cow::Borrowed(DEFAULT_COOKIE_NAME),
            #[cfg(feature = "session")]
            session_key: Cow::Borrowed(DEFAULT_SESSION_KEY),
        }
    }

    /// Consumes one rate limit unit, returning the status.
    pub async fn count(&self, key: impl Into<String>) -> Result<(bool, usize, usize), Error> {
        let key = key.into();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as usize;
        let win = self.period.as_secs() as usize;

        let mut conn = self.pool.get().await?;
        let res: Vec<i64> = redis::cmd("EVAL")
            .arg(LUA)
            .arg(1)                       // number of keys
            .arg(&key)                    // KEYS[1]
            .arg(self.limit as i64)       // ARGV[1]
            .arg(win as i64)              // ARGV[2]
            .arg(now as i64)              // ARGV[3]
            .query_async(&mut *conn)   
            .await?;

        let limited = res[0] == 1;
        let remaining = res[1] as usize;
        let reset = res[2] as usize;
        Ok((limited, remaining, reset))
    }
}