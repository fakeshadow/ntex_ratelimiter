use std::time::Duration;

use ntex::web::{self, App, HttpServer};

#[web::get("/")]
async fn no_params() -> &'static str {
    "Hello world!\r\n"
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    HttpServer::new(move || {
        let rate_limiter = ntex_ratelimiter::default()
            .interval(Duration::from_secs(60))
            .recycle_interval(Duration::from_secs(1))
            .max_requests(360000);

        App::new().wrap(rate_limiter).service(no_params)
    })
    .bind("0.0.0.0:8000")?
    .run()
    .await
}
