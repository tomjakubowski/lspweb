#![warn(clippy::all)]
#![deny(private_in_public)]

use serde::Deserialize;
use std::sync::Arc;
use warp::{self, Filter, Reply};

mod lsp;

use lsp::LsClient;

pub type Lsp = Arc<LsClient>;

fn main() {
    env_logger::init();
    // Initialize language server process
    let lsp = Arc::new(LsClient::start().expect("Language server failed to start"));

    let lsp_filter = cloner(warp::any().map(move || lsp.clone()));
    let debug_route = lsp_filter()
        .and(warp::path("debug"))
        .and(warp::get2())
        .map(handle_debug);
    let symbol_route = lsp_filter()
        .and(warp::path("workspace"))
        .and(warp::path("symbol"))
        .and(warp::query::<GenericQuery>())
        .and(warp::get2())
        .map(handle_workspace_symbol);
    let routes = debug_route.or(symbol_route);
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

#[derive(Deserialize)]
struct GenericQuery {
    q: String,
}

fn handle_workspace_symbol(lsp: Lsp, query: GenericQuery) -> impl Reply {
    let symbols = lsp.workspace_symbol(&query.q).unwrap().unwrap();
    let symbol_list = symbols
        .into_iter()
        .map(|sym| format!("{:?}", sym))
        .collect::<Vec<_>>()
        .join("\n");
    // FIXME: this should be escaped i.e. use tera or whatever
    let resp = format!(
        r#"
    <!doctype html>
    <html>
    <head>
        <title>debug</title>
    </head>
    <body>
        <form method="get">
            <label for="q">Search: </label>
            <input type="text" name="q" value="{query}" />
        </form>
        <h2>symbols</h2>
        <pre>
{symbols}
        </pre>
    </body>
    </html>
    "#,
        query = query.q,
        symbols = symbol_list
    );
    warp::reply::html(resp)
}

fn handle_debug(lsp: Lsp) -> impl Reply {
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
