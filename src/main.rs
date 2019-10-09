#![warn(clippy::all)]
#![deny(private_in_public)]

use std::sync::{Arc, Mutex};
use warp::{self, Filter, Reply};

mod lsp;

use lsp::LsClient;

pub type Lsp = Arc<Mutex<LsClient>>;

fn main() {
    env_logger::init();
    // Initialize language server process
    let lsp = Arc::new(Mutex::new(
        LsClient::start().expect("Language server failed to start"),
    ));

    let lsp_filter = cloner(warp::any().map(move || lsp.clone()));
    let debug_route = lsp_filter()
        .and(warp::path("debug"))
        .and(warp::get2())
        .map(handle_debug);
    let symbols_route = lsp_filter()
        .and(warp::path("symbols"))
        .and(warp::get2())
        .map(handle_symbols);
    let routes = debug_route.or(symbols_route);
    const PORT: u16 = 8100;
    let allowed_hosts = [format!("127.0.0.1:{}", PORT), format!("localhost:{}", PORT)];
    let app = host_filter(move |host: &str| allowed_hosts.iter().any(|h| h == host)).and(routes);

    log::info!("Serving");
    warp::serve(app).run(([127, 0, 0, 1], PORT));
}

fn cloner<T>(cl: T) -> impl Fn() -> T
where
    T: Clone,
{
    move || cl.clone()
}

fn host_filter<F>(hostp: F) -> warp::filters::BoxedFilter<()>
where
    F: Fn(&str) -> bool + Clone + Send + Sync + 'static,
{
    warp::header("Host")
        .and_then(move |req_host: String| {
            if hostp(&req_host) {
                Ok(())
            } else {
                Err(warp::reject::custom("Invalid Host header"))
            }
        })
        .untuple_one()
        .boxed()
}

fn handle_symbols(lsp: Lsp) -> impl Reply {
    let _lsp = lsp.lock().expect("lock failure");
    "Foo"
}

fn handle_debug(lsp: Lsp) -> impl Reply {
    let lsp = lsp.lock().expect("lock failure");
    let resp = format!(
        r#"
    <!doctype html>
    <html>
    <head>
        <title>debug</title>
    </head>
    <body>
        <pre>
language server pid: {}
        </pre>
    </body>
    </html>
    "#,
        lsp.pid()
    );
    warp::reply::html(resp)
}
