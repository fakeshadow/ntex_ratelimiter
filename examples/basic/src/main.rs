use std::time::Duration;

use ntex::web::{self, App, HttpServer};

use ntex_ratelimiter::RateLimiter;

#[web::get("/")]
async fn no_params() -> &'static str {
    "Hello world!\r\n"
}

#[ntex_rt::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        App::new()
            .wrap(
                RateLimiter::new()
                    .interval(Duration::from_secs(60))
                    .recycle_interval(Duration::from_secs(1))
                    .max_requests(360000),
            )
            .service(no_params)
    })
    .bind("0.0.0.0:8081")?
    .run()
    .await
}
