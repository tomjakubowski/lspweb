#![warn(clippy::all)]
#![deny(private_in_public)]

use serde::Deserialize;
use std::sync::Arc;
use tera::{Context as TeraContext, Tera};
use warp::{self, path, Filter, Reply};

mod lsp;
mod templates;

use lsp::LsClient;

pub type Lsp = Arc<LsClient>;

fn main() {
    env_logger::init();
    let cwd = std::env::current_dir().unwrap();
    let lsp = Arc::new(LsClient::start(&cwd).expect("Language server failed to start"));
    let tera = Arc::new(templates::make_tera());
    let lsp_filter = cloner(warp::any().map(move || lsp.clone()));
    let tera_filter = cloner(warp::any().map(move || tera.clone()));

    let debug_route = path!("debug")
        .and(warp::path::end())
        .and(lsp_filter())
        .and(tera_filter())
        .and(warp::get2())
        .map(handle_debug);
    let doc_route = path!("d")
        .and(warp::path::tail())
        .and(lsp_filter())
        .and(tera_filter())
        .and(warp::get2())
        .map(handle_document);
    let symbol_route = path!("workspace" / "symbol")
        .and(warp::path::end())
        .and(lsp_filter())
        .and(tera_filter())
        .and(warp::query::<GenericQuery>())
        .and(warp::get2())
        .map(handle_workspace_symbol);
    let routes = debug_route.or(symbol_route).or(doc_route);
    const PORT: u16 = 8100;
    let allowed_hosts = [format!("127.0.0.1:{}", PORT), format!("localhost:{}", PORT)];
    let app = host_filter(move |host: &str| allowed_hosts.iter().any(|h| h == host)).and(routes);

    log::info!("Serving on http://localhost:8100/");
    warp::serve(app).run(([127, 0, 0, 1], PORT));
}

#[macro_export]
macro_rules! lsp_call {
    ($self:expr, $name:tt, { $($field:ident : $value:expr)*$(,)? }) => {{
        type Req = lsp_types::lsp_request!($name);
        $self.call::<Req>($crate::lsp::ReqParams::<Req> {
            $($field: $value),*
        })
    }};
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
    q: Option<String>,
}

fn handle_workspace_symbol(lsp: Lsp, tera: Arc<Tera>, query: GenericQuery) -> impl Reply {
    let query = query.q.unwrap_or_default();
    let symbols = lsp.workspace_symbol(&query).unwrap().unwrap();
    let mut ctxt = TeraContext::new();
    ctxt.insert("query", &query);
    ctxt.insert("symbols", &symbols);
    let resp = tera
        .render("workspace/symbol.html", ctxt)
        .expect("Template error");
    warp::reply::html(resp)
}

fn handle_document(tail: warp::path::Tail, lsp: Lsp, tera: Arc<Tera>) -> impl Reply {
    let mut ctxt = TeraContext::new();
    ctxt.insert("path", tail.as_str());
    ctxt.insert("content", include_str!("main.rs"));

    // totally bogus
    use lsp_types::{TextDocumentIdentifier, Url};
    let mut url = Url::parse("file://").unwrap();
    let path = lsp.project_dir().join("src/main.rs");
    url.set_path(path.to_str().unwrap());

    let td = TextDocumentIdentifier::new(url);

    let symbols = lsp_call!(lsp, "textDocument/documentSymbol", { text_document: td }).unwrap();
    ctxt.insert("symbols", &symbols);
    let resp = tera.render("document.html", ctxt).expect("Template error");
    warp::reply::html(resp)
}

fn handle_debug(lsp: Lsp, tera: Arc<Tera>) -> impl Reply {
    let mut ctxt = TeraContext::new();
    ctxt.insert("pid", &lsp.pid());
    let resp = tera.render("debug.html", ctxt).expect("Template error");
    warp::reply::html(resp)
}
