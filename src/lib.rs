//! Rate Limiter for ntex.
//!
//! ### Example:
//! ```rust,no_run
//! use std::time::Duration;
//! use ntex::web::{self, App, HttpResponse, Error};
//! use ntex_ratelimiter::RateLimiter;
//!
//! fn index() -> Result<&'static str, Error> {
//!     Ok("Welcome!")
//! }
//!
//! #[ntex::main]
//! async fn main() -> std::io::Result<()> {
//!     web::server(||
//!         App::new()
//!             .wrap(
//!                 RateLimiter::new()
//!                     .max_requests(3600)
//!                      // every remote socket address have 3600 max requests in 3600 seconds.
//!                     .interval(Duration::from_secs(3600))
//!                      // run a future every 60 seconds and remove address exceed rate limit according to interval
//!                     .recycle_interval(Duration::from_secs(60))
//!              )
//!             .service(web::resource("/").to(index)))
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
pub struct RateLimiter<E> {
    interval: Duration,
    recycle_interval: Duration,
    max_requests: u32,
    identifier: Rc<dyn Identifier<E>>,
    filter: Rc<dyn Filter<E>>,
    _t: PhantomData<E>,
}

pub trait Identifier<E> {
    fn identify(&self, req: &WebRequest<E>) -> Option<String>;
}

struct DefaultIdentifier;

impl<E> Identifier<E> for DefaultIdentifier {
    fn identify<'a>(&self, req: &WebRequest<E>) -> Option<String> {
        req.connection_info().remote().map(String::from)
    }
}

pub trait Filter<E> {
    fn filter(&self, req: &WebRequest<E>) -> BoxedFuture<FilterResult>;
}

pub enum FilterResult {
    Skip,
    Continue,
}

struct DefaultFilter;

impl<E> Filter<E> for DefaultFilter {
    fn filter(&self, _req: &WebRequest<E>) -> BoxedFuture<FilterResult> {
        Box::pin(async { FilterResult::Continue })
    }
}

const DEFAULT_INTERVAL: Duration = Duration::from_secs(1800);
const DEFAULT_RECYCLE_INTERVAL: Duration = Duration::from_secs(60);

impl<E> Default for RateLimiter<E> {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            recycle_interval: DEFAULT_RECYCLE_INTERVAL,
            max_requests: 3600,
            identifier: Rc::new(DefaultIdentifier),
            filter: Rc::new(DefaultFilter),
            _t: PhantomData,
        }
    }
}

impl<E> RateLimiter<E> {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn interval(mut self, dur: Duration) -> Self {
        self.interval = dur;
        self
    }

    pub fn recycle_interval(mut self, dur: Duration) -> Self {
        self.recycle_interval = dur;
        self
    }

    pub fn max_requests(mut self, max_requests: u32) -> Self {
        self.max_requests = max_requests;
        self
    }

    pub fn identifier(mut self, identifier: impl Identifier<E> + 'static) -> Self {
        self.identifier = Rc::new(identifier);
        self
    }

    pub fn filter(mut self, filter: impl Filter<E> + 'static) -> Self {
        self.filter = Rc::new(filter);
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

impl<S, E> Transform<S> for RateLimiter<E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse> + 'static,
    S::Future: 'static,
    S::Error: 'static,
    E: ErrorRenderer,
    E::Container: From<RateLimiterError>,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Transform = RateLimitMiddleware<S, E>;
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

pub struct RateLimitMiddleware<S, E> {
    service: Rc<S>,
    max_requests: u32,
    identifier: Rc<dyn Identifier<E>>,
    filter: Rc<dyn Filter<E>>,
    _t: PhantomData<E>,
}

impl<S, E> Service for RateLimitMiddleware<S, E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse> + 'static,
    S::Future: 'static,
    S::Error: 'static,
    E: ErrorRenderer,
    E::Container: From<RateLimiterError>,
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
        let filter = self.filter.clone();
        let service = self.service.clone();

        Box::pin(async move {
            match filter.filter(&req).await {
                FilterResult::Continue => {
                    let rate = id.map(|id| {
                        let mut map = map().lock();

                        let entry = map.get_mut(&id);
                        match entry {
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
                        Some(Ok(rate)) => rate,
                        Some(Err(e)) => return Ok(req.error_response(e)),
                        None => return Ok(req.error_response(RateLimiterError::RemoteAddress)),
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
                FilterResult::Skip => service.call(req).await,
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
        tokio::spawn(async move {
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
