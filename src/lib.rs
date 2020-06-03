//! Rate Limiter for ntex.
//!
//! ### Example:
//! ```rust,no_run
//! use std::time::Duration;
//! use ntex::web::{self, App, HttpResponse, Error};
//!
//! #[web::get("/")]
//! async fn index() -> &'static str {
//!     "Welcome!"
//! }
//!
//! #[ntex::main]
//! async fn main() -> std::io::Result<()> {
//!     web::server(||
//!         App::new()
//!             .wrap(
//!                 ntex_ratelimiter::default()
//!                     .max_requests(3600)
//!                      // every remote socket address have 3600 max requests in 3600 seconds.
//!                     .interval(Duration::from_secs(3600))
//!                      // run a future every 60 seconds and remove address exceed rate limit according to interval
//!                     .recycle_interval(Duration::from_secs(60))
//!              )
//!             .service(index)
//!      )
//!      .bind("127.0.0.1:59880")?
//!      .run()
//!      .await
//! }
//! ```

use std::convert::From;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use derive_more::{Display, From};
use fxhash::FxHashMap;
use ntex::http::{
    header::{HeaderName, HeaderValue},
    StatusCode,
};
use ntex::service::{Service, Transform};
use ntex::web::{
    dev::{WebRequest, WebResponse},
    DefaultError, ErrorRenderer, WebResponseError,
};
use once_cell::sync::OnceCell;
use parking_lot::Mutex;

#[derive(Clone)]
pub struct RateLimiter<E, I, F>
where
    I: Identifier<E> + Clone + 'static,
    F: Filter<E> + Clone + 'static,
{
    interval: Duration,
    recycle_interval: Duration,
    max_requests: u32,
    identifier: I,
    filter: F,
    _t: PhantomData<E>,
}

pub trait Identifier<E> {
    fn identify(&self, req: &WebRequest<E>) -> Option<String>;
}

pub trait Filter<E> {
    fn filter(&self, req: &WebRequest<E>) -> BoxedFuture<FilterResult>;
}

pub enum FilterResult {
    Skip,
    Continue,
}

#[derive(Clone)]
pub struct DefaultIdentifier;

impl<E> Identifier<E> for DefaultIdentifier {
    fn identify<'a>(&self, req: &WebRequest<E>) -> Option<String> {
        req.connection_info().remote().map(String::from)
    }
}

#[derive(Clone)]
pub struct DefaultFilter;

impl<E> Filter<E> for DefaultFilter {
    fn filter(&self, _req: &WebRequest<E>) -> BoxedFuture<FilterResult> {
        Box::pin(async { FilterResult::Continue })
    }
}

/// shortcut for default rate limiter with no filter and remote socket address as identifier.
pub fn default<E>() -> RateLimiter<E, DefaultIdentifier, DefaultFilter> {
    RateLimiter::new(DefaultIdentifier, DefaultFilter)
}

const DEFAULT_INTERVAL: Duration = Duration::from_secs(1800);
const DEFAULT_RECYCLE_INTERVAL: Duration = Duration::from_secs(60);

impl<E, I, F> RateLimiter<E, I, F>
where
    I: Identifier<E> + Clone + 'static,
    F: Filter<E> + Clone + 'static,
{
    /// construct a new rate limiter with certain identifier and filter.
    pub fn new(identifier: I, filter: F) -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            recycle_interval: DEFAULT_RECYCLE_INTERVAL,
            max_requests: 3600,
            identifier,
            filter,
            _t: PhantomData,
        }
    }

    /// determine the duration of every cycle of rate limit
    pub fn interval(mut self, dur: Duration) -> Self {
        self.interval = dur;
        self
    }

    /// determine the duration of periodically free clients exceeds max rate limit.
    pub fn recycle_interval(mut self, dur: Duration) -> Self {
        self.recycle_interval = dur;
        self
    }

    /// determine the max requests count for one client at every cycle.
    pub fn max_requests(mut self, max_requests: u32) -> Self {
        self.max_requests = max_requests;
        self
    }
}

#[derive(Debug, From, Display)]
pub enum RateLimiterError {
    #[display(fmt = "Fail to extract remote address from Request")]
    RemoteAddress,
    #[display(fmt = "Rate limit has been reached.")]
    TooManyRequests,
}

impl WebResponseError<DefaultError> for RateLimiterError {
    fn status_code(&self) -> StatusCode {
        match self {
            RateLimiterError::RemoteAddress => StatusCode::BAD_REQUEST,
            RateLimiterError::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

type BoxedFuture<T> = Pin<Box<dyn Future<Output = T>>>;

impl<S, E, I, F> Transform<S> for RateLimiter<E, I, F>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse> + 'static,
    S::Future: 'static,
    S::Error: 'static,
    E: ErrorRenderer,
    E::Container: From<RateLimiterError>,
    I: Identifier<E> + Clone + 'static,
    F: Filter<E> + Clone + 'static,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Transform = RateLimitMiddleware<S, E, I, F>;
    type InitError = ();
    type Future = BoxedFuture<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        // signal recycle to start
        let _tx = recycle(self.interval, self.recycle_interval);
        let max_requests = self.max_requests;
        let identifier = self.identifier.clone();
        let filter = self.filter.clone();

        Box::pin(async move {
            Ok(RateLimitMiddleware {
                service: Rc::new(service),
                max_requests,
                identifier,
                filter,
                _t: PhantomData,
            })
        })
    }
}

pub struct RateLimitMiddleware<S, E, I, F>
where
    I: Identifier<E> + Clone + 'static,
    F: Filter<E> + Clone + 'static,
{
    service: Rc<S>,
    max_requests: u32,
    identifier: I,
    filter: F,
    _t: PhantomData<E>,
}

impl<S, E, I, F> Service for RateLimitMiddleware<S, E, I, F>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse> + 'static,
    S::Future: 'static,
    S::Error: 'static,
    E: ErrorRenderer,
    E::Container: From<RateLimiterError>,
    I: Identifier<E> + Clone + 'static,
    F: Filter<E> + Clone + 'static,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Future = BoxedFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn poll_shutdown(&self, cx: &mut Context, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, req: WebRequest<E>) -> Self::Future {
        let max_requests = self.max_requests;
        let id = self.identifier.identify(&req);
        let filter = self.filter.filter(&req);
        let service = self.service.clone();

        Box::pin(async move {
            match filter.await {
                FilterResult::Skip => service.call(req).await,

                FilterResult::Continue => {
                    let rate = id.ok_or(RateLimiterError::RemoteAddress).and_then(|id| {
                        let mut map = map().lock();

                        match map.get_mut(&id) {
                            Some((count, _)) => {
                                if *count > 0 {
                                    *count -= 1;
                                    Ok(*count)
                                } else {
                                    Err(RateLimiterError::TooManyRequests)
                                }
                            }
                            None => {
                                let rate = max_requests - 1;
                                map.insert(id.to_owned(), (rate, Instant::now()));
                                Ok(rate)
                            }
                        }
                    });

                    let rate = match rate {
                        Ok(rate) => rate,
                        Err(e) => return Ok(req.error_response(e)),
                    };

                    let mut res = service.call(req).await?;

                    let headers = res.headers_mut();
                    headers.insert(
                        HeaderName::from_static("x-ratelimit-limit"),
                        HeaderValue::from_str(max_requests.to_string().as_str()).unwrap(),
                    );
                    headers.insert(
                        HeaderName::from_static("x-ratelimit-remaining"),
                        HeaderValue::from_str(rate.to_string().as_str()).unwrap(),
                    );

                    Ok(res)
                }
            }
        })
    }
}

// global map
struct RateMap(Mutex<FxHashMap<String, (u32, Instant)>>);

impl Default for RateMap {
    fn default() -> Self {
        Self(Mutex::new(FxHashMap::default()))
    }
}

fn map() -> &'static Mutex<FxHashMap<String, (u32, Instant)>> {
    static MAP: OnceCell<RateMap> = OnceCell::new();
    &MAP.get_or_init(RateMap::default).0
}

// guard for recycle future.
struct RateRecycle;

fn recycle(interval: Duration, recycle_interval: Duration) -> &'static RateRecycle {
    static GC: OnceCell<RateRecycle> = OnceCell::new();
    GC.get_or_init(|| {
        let mut recycle_interval = tokio::time::interval(recycle_interval);
        tokio::task::spawn_local(async move {
            loop {
                let _ = recycle_interval.tick().await;
                let now = Instant::now();
                map().lock().retain(|_, (count, instant)| {
                    now.duration_since(*instant) < interval || *count > 0
                });
            }
        });
        RateRecycle
    })
}
