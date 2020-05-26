use std::future::Future;
use std::pin::Pin;

use ntex::web::{self, dev::WebRequest, App, HttpServer};

use ntex_ratelimiter::{Filter, FilterResult, Identifier, RateLimiter};

#[web::get("/")]
async fn no_params() -> &'static str {
    "Hello world!\r\n"
}

#[ntex_rt::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            .wrap(RateLimiter::new().identifier(MyIdentifier).filter(MyFilter))
            .service(no_params)
    })
    .bind("0.0.0.0:8081")?
    .run()
    .await
}

// use custom trait object as identifier for rate limit middleware.
struct MyIdentifier;

impl<Any> Identifier<Any> for MyIdentifier {
    fn identify(&self, req: &WebRequest<Any>) -> Option<String> {
        req.headers()
            .get("user_identify")
            .map(|r| r.to_str().unwrap().to_owned())

        // if we return None in this method the request will be rejected and a 400 BadRequest error will return as response.
    }
}

// custom filter to skip rate limiter
struct MyFilter;

impl<Any> Filter<Any> for MyFilter {
    fn filter(&self, req: &WebRequest<Any>) -> Pin<Box<dyn Future<Output = FilterResult>>> {
        // extract some token from request header
        let token = req
            .headers()
            .get("Authorization")
            .map(|r| r.to_str().unwrap().to_owned());

        Box::pin(async move {
            if let Some(token) = token {
                if token.contains("some auth token") {
                    // return to skip the rate limiter.
                    return FilterResult::Skip;
                }
            }

            // otherwise we just continue to the process of rate limiter
            FilterResult::Continue
        })
    }
}
