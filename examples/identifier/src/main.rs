use ntex::web::dev::WebRequest;
use ntex::web::{self, App, HttpServer};
use ntex_ratelimiter::{Identifier, RateLimiter};

#[web::get("/")]
async fn no_params() -> &'static str {
    "Hello world!\r\n"
}

#[ntex_rt::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            .wrap(RateLimiter::new().identifier(MyIdentifier))
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
