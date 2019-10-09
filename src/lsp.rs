use channel::Sender;
use crossbeam::channel;
use lsp_types::{
    self, lsp_notification, lsp_request, notification::Notification, request::Request,
    InitializeParams, InitializedParams,
};
use serde::{Deserialize, Serialize};
use serde_json;
use serde_json::Value as Json;
use std::{
    collections::BTreeMap,
    io::{self, BufReader},
    process::{self, Command, Stdio},
    thread::JoinHandle,
};

pub struct LsClient {
    process: process::Child,
    reader_thread: JoinHandle<()>,
    readerctl_tx: Sender<ReaderControl>,
    writer_thread: JoinHandle<()>,
    writer_tx: Sender<WriterControl>,
}

enum ReaderControl {
    Await(u64, Sender<RpcResponse>),
}
enum WriterControl {
    Request(RpcRequest),
    Shutdown,
}

pub type ReqParams<R> = <R as Request>::Params;
pub type ReqResult<R> = <R as Request>::Result;
pub type NotifyParams<N> = <N as Notification>::Params;

#[derive(Debug)]
pub enum CallError {
    RpcError(RpcError),
    IoError(io::Error),
}

pub type CallResult<R> = Result<ReqResult<R>, CallError>;

impl LsClient {
    pub fn start() -> io::Result<LsClient> {
        let mut server = Command::new("ra_lsp_server")
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()?;
        log::trace!("Language server process started, pid {}", server.id());
        let mut server_stdin = server.stdin.take().unwrap();
        // To shut it down: send message to the writer thread to stop; writer thread closes stdin
        // of server process, which should cause it to exit and close stdout; which will quit the
        // reader thread.
        let (writer_tx, writer_rx) = channel::unbounded::<WriterControl>();
        let writer_thread = std::thread::spawn(move || loop {
            let msg = match writer_rx.recv() {
                Ok(m) => m,
                Err(e) => {
                    log::error!("writer_rx recv(): {}", e);
                    break;
                }
            };
            match msg {
                WriterControl::Request(req) => {
                    use std::io::Write;
                    log::trace!("Writing request");
                    let json = serde_json::to_string(&req).unwrap();
                    write!(
                        server_stdin,
                        "Content-Length: {}\r\n\r\n{}",
                        json.len(),
                        json
                    )
                    .unwrap();
                    server_stdin.flush().unwrap();
                }
                WriterControl::Shutdown => {
                    log::trace!("Shutting down writer thread");
                    break;
                }
            }
        });

        let mut server_stdout = BufReader::new(server.stdout.take().unwrap());
        let (readerctl_tx, readerctl_rx) = channel::unbounded::<ReaderControl>();
        let mut awaiters = BTreeMap::new();
        let reader_thread = std::thread::spawn(move || loop {
            use std::io::{BufRead, Read};

            let mut read_buf = String::new();
            server_stdout.read_line(&mut read_buf).unwrap();
            let content_len = parse_content_length(&read_buf).expect("Content-Length parse failed");
            server_stdout.read_line(&mut read_buf).unwrap();
            let mut read_buf = vec![0u8; content_len];
            server_stdout.read_exact(&mut read_buf).unwrap();
            log::trace!("{:?}", std::str::from_utf8(&read_buf).unwrap());
            let res: RpcResponse = serde_json::from_slice(&read_buf).unwrap();
            match res.id {
                None => {
                    log::info!("Read notification:\n{:?}", res);
                    // Throw notifications away for now
                }
                Some(msg_id) => {
                    // 1. Drain readerctl_rx
                    for msg in readerctl_rx.try_iter() {
                        match msg {
                            ReaderControl::Await(id, tx) => {
                                log::trace!("Drained awaiter for id {}", id);
                                awaiters.insert(id, tx);
                            }
                        }
                    }
                    log::trace!("Read response for id {}", msg_id);
                    match awaiters.remove(&msg_id) {
                        Some(response_tx) => response_tx.send(res).unwrap(),
                        None => {
                            log::error!(
                                "Received response for request {} with no awaiter.\n{:?}",
                                msg_id,
                                res
                            );
                        }
                    }
                }
            }
        });

        let client = LsClient {
            process: server,
            reader_thread,
            readerctl_tx,
            writer_thread,
            writer_tx,
        };

        let cwd = std::env::current_dir().unwrap();
        let workspace_dir: &str = cwd.to_str().unwrap();
        client
            .call::<lsp_request!("initialize")>(initialize_params(workspace_dir))
            .unwrap();

        client.notify::<lsp_notification!("initialized")>(InitializedParams {});
        Ok(client)
    }

    pub fn call<M>(&self, params: ReqParams<M>) -> CallResult<M>
    where
        M: Request,
        for<'de> ReqResult<M>: Deserialize<'de>,
        ReqParams<M>: Serialize,
    {
        let id = self.next_id();
        let request = RpcRequest {
            id: Some(id),
            method: M::METHOD,
            params: serde_json::to_value(params).unwrap(),
        };
        let (res_tx, res_rx) = channel::bounded(0);
        self.readerctl_tx
            .send(ReaderControl::Await(id, res_tx))
            .unwrap();
        self.writer_tx
            .send(WriterControl::Request(request))
            .unwrap();
        let res = res_rx.recv().unwrap().payload;
        res.into_call_result::<M>()
    }

    pub fn notify<N>(&self, params: NotifyParams<N>)
    where
        N: Notification,
        NotifyParams<N>: Serialize,
    {
        let request = RpcRequest {
            id: None,
            method: N::METHOD,
            params: serde_json::to_value(params).unwrap(),
        };
        self.writer_tx
            .send(WriterControl::Request(request))
            .unwrap();
    }

    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    pub fn next_id(&self) -> u64 {
        // FIXME
        0
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim_end();
    let needle = "Content-Length: ";
    let mid = line.find(needle)? + needle.len();
    let (_, num) = line.split_at(mid);
    num.parse().ok()
}

#[test]
fn test_parse_content_length() {
    assert_eq!(37, parse_content_length("Content-Length: 37").unwrap());
}

#[derive(Debug, Serialize)]
struct RpcRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    method: &'static str,
    params: Json,
}

#[derive(Debug, Serialize)]
struct RpcNotification {
    method: &'static str,
    params: Json,
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
    Result(Json),
    #[serde(rename = "error")]
    Error(RpcError),
}

impl ResponsePayload {
    fn into_call_result<M>(self) -> CallResult<M>
    where
        M: Request,
        for<'de> ReqResult<M>: Deserialize<'de>,
    {
        match self {
            ResponsePayload::Result(val) => Ok(serde_json::from_value(val).unwrap()),
            ResponsePayload::Error(err) => Err(CallError::RpcError(err)),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct RpcError {
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
            payload: ResponsePayload::Result(Json::String("nice".to_string()))
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

fn initialize_params(workspace_path: &str) -> InitializeParams {
    use lsp_types::{ClientCapabilities, TraceOption, Url};
    let mut url = Url::parse("file:///").unwrap();
    url.set_path(workspace_path);
    InitializeParams {
        process_id: Some(u64::from(std::process::id())),
        root_path: Some(workspace_path.to_string()),
        root_uri: Some(url),
        initialization_options: None,
        capabilities: ClientCapabilities::default(),
        trace: Some(TraceOption::Verbose),
        workspace_folders: None,
    }
}
