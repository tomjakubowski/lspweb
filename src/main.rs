use lsp_types;
use serde::{Deserialize, Serialize};
use serde_json;
use std::{
    io::{self, BufReader},
    process::{self, Command, Stdio},
    sync::{Arc, Mutex},
};
use warp::{self, Filter, Reply};

type Lsp = Arc<Mutex<LsProcess>>;

fn main() {
    env_logger::init();
    // Initialize language server process
    let lsp = Arc::new(Mutex::new(
        LsProcess::start().expect("Language server failed to start"),
    ));

    let lsp_filter = warp::any().map(move || lsp.clone());
    let debug_route = lsp_filter
        .and(warp::path("debug"))
        .and(warp::get2())
        .map(handle_debug);
    let routes = debug_route; // or...
    const PORT: u16 = 8100;
    let allowed_hosts = [
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        format!("127.0.0.1:{}", PORT),
        format!("localhost:{}", PORT),
    ];
    let app = host_filter(move |host: &str| allowed_hosts.iter().any(|h| h == host)).and(routes);

    log::info!("Serving");
    warp::serve(app).run(([127, 0, 0, 1], PORT));
}

fn host_filter<'a, F>(hostp: F) -> warp::filters::BoxedFilter<()>
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
    buf_stdout: BufReader<process::ChildStdout>,
    stdin: process::ChildStdin,
    proc: process::Child,
    seqnum: u64,
}

impl LsProcess {
    pub fn start() -> io::Result<LsProcess> {
        let mut lsp: LsProcess = {
            let mut proc = Command::new("ra_lsp_server")
                .stdin(Stdio::piped())
                .stderr(Stdio::inherit())
                .stdout(Stdio::piped())
                .spawn()?;
            let stdin = proc.stdin.take().unwrap();
            let buf_stdout = BufReader::new(proc.stdout.take().unwrap());
            LsProcess {
                buf_stdout,
                stdin,
                proc,
                seqnum: 0,
            }
        };

        let cwd = std::env::current_dir().unwrap();
        let workspace_dir: &str = cwd.to_str().unwrap();
        let init_req = RpcRequest {
            id: lsp.seqnum,
            method: "initialize",
            params: initialize_params(workspace_dir),
        };
        lsp.seqnum += 1;

        let init_json = serde_json::to_value(init_req).unwrap();
        use std::io::Write;
        let stdin = &mut lsp.stdin;
        let req = wrap_ls_request(&init_json).unwrap();
        log::trace!("writing request:\n{}", req);
        write!(stdin, "{}", req)?;
        stdin.flush().unwrap();

        let mut read_buf = String::new();
        use std::io::{BufRead, Read};
        lsp.buf_stdout.read_line(&mut read_buf)?;
        let content_len = parse_content_length(&read_buf).expect("Content-Length parse failed");
        lsp.buf_stdout.read_line(&mut read_buf)?;
        let mut read_buf = vec![0u8; content_len];
        lsp.buf_stdout.read_exact(&mut read_buf)?;
        let res: RpcResponse = serde_json::from_slice(&read_buf).unwrap();
        log::trace!("RpcResponse:\n\n{:#?}", res);
        // log::trace!("Json:\n\n{}", String::from_utf8(read_buf).unwrap());

        Ok(lsp)
    }

    // fn write_request(&mut self, method: &'static str, params: serde_json::Value) {}

    pub fn pid(&self) -> u32 {
        self.proc.id()
    }
}

impl Drop for LsProcess {
    fn drop(&mut self) {
        self.proc.wait().expect("Wait error");
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim_end();
    let needle = "Content-Length: ";
    let mid = line.find(needle)? + needle.len();
    let (_, num) = line.split_at(mid);
    println!("num: {}", num);
    num.parse().ok()
}

#[test]
fn test_parse_content_length() {
    assert_eq!(37, parse_content_length("Content-Length: 37").unwrap());
}

#[derive(Debug, Serialize)]
struct RpcRequest {
    id: u64,
    method: &'static str,
    params: serde_json::Value,
}

#[derive(Debug, Deserialize, PartialEq)]
struct RpcResponse {
    id: Option<u64>,
    #[serde(flatten)]
    payload: ResponsePayload,
}

#[derive(Debug, Deserialize, PartialEq)]
enum ResponsePayload {
    #[serde(rename = "result")]
    Result(serde_json::Value),
    #[serde(rename = "error")]
    Error(RpcError),
}

#[derive(Debug, Deserialize, PartialEq)]
struct RpcError {
    code: i64,
    message: String,
}

#[test]
fn test_deserialize_rpc_response() {
    let json = r#"{
      "jsonrpc": "2.0",
      "id": 69,
      "result": "nice"
    }"#;
    assert_eq!(
        RpcResponse {
            id: Some(69),
            payload: ResponsePayload::Result(serde_json::Value::String("nice".to_string()))
        },
        serde_json::from_str(json).unwrap()
    );
    let json = r#"{
      "jsonrpc": "2.0",
      "id": 69,
      "error": {
        "code": -420,
        "message": "not nice"
      }
    }"#;
    assert_eq!(
        RpcResponse {
            id: Some(69),
            payload: ResponsePayload::Error(RpcError {
                code: -420,
                message: "not nice".to_string()
            })
        },
        serde_json::from_str(json).unwrap()
    );
}

fn initialize_params(workspace_path: &str) -> serde_json::Value {
    use lsp_types::{ClientCapabilities, InitializeParams, TraceOption, Url};
    let mut url = Url::parse("file:///").unwrap();
    url.set_path(workspace_path);
    serde_json::to_value(InitializeParams {
        process_id: Some(std::process::id() as u64),
        root_path: Some(workspace_path.to_string()),
        root_uri: Some(url),
        initialization_options: None,
        capabilities: ClientCapabilities::default(),
        trace: Some(TraceOption::Verbose),
        workspace_folders: None,
    })
    .unwrap()
}

fn wrap_ls_request(val: &serde_json::Value) -> Result<String, serde_json::Error> {
    let json = serde_json::to_string(&val)?;
    Ok(format!("Content-Length: {}\r\n\r\n{}", json.len(), json))
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
