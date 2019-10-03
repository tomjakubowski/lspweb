use crossbeam::channel as chan;
use lsp_types;
use serde::Serialize;
use serde_json;
use std::{
    io,
    path::Path,
    process::{self, Command, Stdio},
    sync::{Arc, Mutex},
};
use warp::{self, Filter, Reply};

type Lsp = Arc<Mutex<LsProcess>>;

fn main() {
    // Initialize language server process
    let lsp = Arc::new(Mutex::new(
        LsProcess::start().expect("Language server failed to start"),
    ));
    let lsp_filter = warp::any().map(move || lsp.clone());

    let app = warp::path("debug")
        .and(lsp_filter)
        .and(warp::get2())
        .map(handle_debug);
    warp::serve(app).run(([127, 0, 0, 1], 8100));
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

#[derive(Debug)]
struct LsProcess {
    proc: process::Child,
    seqnum: u64,
}

impl LsProcess {
    pub fn start() -> io::Result<LsProcess> {
        let mut proc = Command::new("ra_lsp_server")
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut lsp = LsProcess { proc, seqnum: 0 };

        let cwd = std::env::current_dir().unwrap();
        let workspace_dir: &str = cwd.to_str().unwrap();
        let init_req = RpcRequest {
            id: lsp.seqnum,
            method: "initialize",
            params: serde_json::to_value(initialize_params(workspace_dir)).unwrap(),
        };
        lsp.seqnum += 1;
        let init_json = serde_json::to_value(init_req).unwrap();
        use std::io::Write;
        let stdin = lsp.proc.stdin.as_mut().unwrap();
        write!(stdin, "{}", wrap_ls_request(&init_json).unwrap()).unwrap();
        stdin.flush().unwrap();

        Ok(lsp)
    }

    fn write_request(&mut self, method: &'static str, params: serde_json::Value) {}

    pub fn pid(&self) -> u32 {
        self.proc.id()
    }
}

impl Drop for LsProcess {
    fn drop(&mut self) {
        self.proc.wait().expect("Wait error");
    }
}

#[derive(Debug, Serialize)]
struct RpcRequest {
    id: u64,
    method: &'static str,
    params: serde_json::Value,
}

fn initialize_params(workspace_path: &str) -> lsp_types::InitializeParams {
    use lsp_types::{ClientCapabilities, InitializeParams, TraceOption, Url};
    let mut url = Url::parse("file:///").unwrap();
    url.set_path(workspace_path);
    InitializeParams {
        process_id: Some(std::process::id() as u64),
        root_path: Some(workspace_path.to_string()),
        root_uri: Some(url),
        initialization_options: None,
        capabilities: ClientCapabilities::default(),
        trace: Some(TraceOption::Verbose),
        workspace_folders: None,
    }
}

fn wrap_ls_request(val: &serde_json::Value) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(&val)?;
    Ok(format!("Content-Length: {}\r\n{}", json.len(), json))
}

// #[derive(Debug)]
// enum LsCommand {
//     Shutdown,
// }

// struct LsThread {
//     sender: chan::Sender<LsCommand>,
//     joiner: Option<std::thread::JoinHandle<()>>,
// }

// impl LsThread {
//     pub fn start() -> LsThread {
//         let (sender, receiver) = chan::unbounded();
//         let joiner = Some(std::thread::spawn(move || loop {
//             use std::io::Write;

//             let proc = lsp_spawn().expect("lsp failed to start");
//             let mut lsp_stdin = proc.stdin.expect("lsp stdin missing");
//             let mut lsp_stdout = proc.stdout.expect("lsp stdout missing");
//             // TODO: Need to wait on ra_lsp_server process

//             write!(lsp_stdin, "{{}}").expect("lsp write error");

//             // Loop waiting for control messages from web threads
//             match receiver.recv() {
//                 Ok(LsCommand::Shutdown) => {
//                     println!("Received shutdown, exiting");
//                     return;
//                 }
//                 Ok(msg) => println!("Received {:?}", msg),
//                 Err(err) => {
//                     eprintln!("Error: {:?}\nstopping lang server thread", err);
//                     return;
//                 }
//             }
//         }));
//         LsThread { sender, joiner }
//     }
// }

// impl Drop for LsThread {
//     fn drop(&mut self) {
//         let _ignored = self.sender.send(LsCommand::Shutdown);
//         self.joiner
//             .take()
//             .expect("Language server thread already joined?")
//             .join()
//             .expect("language server thread panicked");
//     }
// }
