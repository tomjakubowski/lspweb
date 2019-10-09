use channel::Sender;
use crossbeam::channel;
use lsp_types::{
    self, lsp_notification, lsp_request, notification::Notification, request::Request,
    InitializeParams, InitializedParams,
};
use serde::{Deserialize, Serialize};
use serde_json::{self, Value as Json};
use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::{self, BufReader},
    process::{self, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread::JoinHandle,
};

pub struct LsClient {
    request_counter: AtomicU64,
    process: process::Child,
    reader_thread: JoinHandle<()>,
    readerctl_tx: Sender<ReaderControl>,
    writer_thread: JoinHandle<()>,
    writer_tx: Sender<WriterControl>,
}

enum ReaderControl {
    Await(u64, Sender<Result<Json, RpcError>>),
}
enum WriterControl {
    Write(RawJsonRpc),
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
                WriterControl::Write(req) => {
                    use std::io::Write;
                    log::trace!("Writing JSON RPC");
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

            log::trace!("Enter top of reader loop");
            let mut read_buf = String::new();
            server_stdout.read_line(&mut read_buf).unwrap();
            let content_len = parse_content_length(&read_buf).expect("Content-Length parse failed");
            server_stdout.read_line(&mut read_buf).unwrap();
            let mut read_buf = vec![0u8; content_len];
            server_stdout.read_exact(&mut read_buf).unwrap();
            let msg: RawJsonRpc = serde_json::from_slice(&read_buf).unwrap();
            match msg {
                // Handle notifications
                RawJsonRpc::Call {
                    id: None,
                    method,
                    params,
                } => {
                    // Throw them away for now
                    log::info!("Read notification:\nmethod={}\nparams={}", method, params);
                }
                // Handle responses, whether error or result
                RawJsonRpc::Result { id: msg_id, .. }
                | RawJsonRpc::Error {
                    id: Some(msg_id), ..
                } => {
                    for msg in readerctl_rx.try_iter() {
                        match msg {
                            ReaderControl::Await(await_id, tx) => {
                                log::trace!("Drained awaiter for id {}", await_id);
                                awaiters.insert(await_id, tx);
                            }
                        }
                    }
                    log::trace!("Read response for id {}", msg_id);
                    match awaiters.remove(&msg_id) {
                        Some(response_tx) => response_tx.send(msg.unwrap_into_result()).unwrap(),
                        None => {
                            log::error!(
                                "Received response for request {} with no awaiter.\n{:?}",
                                msg_id,
                                msg
                            );
                        }
                    }
                }
                _ => {
                    log::error!("Read unhandled LSP message:\n{:?}", msg);
                }
            }
        });

        let client = LsClient {
            request_counter: 0.into(),
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
        let id = self.next_request_id();
        let request = RawJsonRpc::request(id, M::METHOD, params);
        let (res_tx, res_rx) = channel::bounded(0);
        self.readerctl_tx
            .send(ReaderControl::Await(id, res_tx))
            .unwrap();
        self.writer_tx.send(WriterControl::Write(request)).unwrap();
        let res = res_rx.recv().unwrap();
        match res {
            Ok(val) => Ok(serde_json::from_value(val).unwrap()),
            Err(e) => Err(CallError::RpcError(e)),
        }
    }

    pub fn notify<N>(&self, params: NotifyParams<N>)
    where
        N: Notification,
        NotifyParams<N>: Serialize,
    {
        let request = RawJsonRpc::notification(N::METHOD, params);
        self.writer_tx.send(WriterControl::Write(request)).unwrap();
    }

    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::Relaxed)
    }
}

fn parse_content_length(line: &str) -> Option<usize> {
    let line = line.trim_end();
    let needle = "Content-Length: ";
    let mid = line.find(needle)? + needle.len();
    let (_, num) = line.split_at(mid);
    num.parse().ok()
}

#[derive(Debug, Deserialize, PartialEq)]
/// A JSON-RPC 2.0 message
#[serde(untagged)]
enum RawJsonRpc {
    Call {
        id: Option<u64>,
        method: Cow<'static, str>,
        params: Json,
    },
    Result {
        id: u64,
        result: Json,
    },
    Error {
        id: Option<u64>,
        error: RpcError,
    },
}

impl RawJsonRpc {
    fn request<P>(id: u64, method: &'static str, params: P) -> Self
    where
        P: Serialize,
    {
        RawJsonRpc::Call {
            id: Some(id),
            method: Cow::Borrowed(method),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    fn notification<P>(method: &'static str, params: P) -> Self
    where
        P: Serialize,
    {
        RawJsonRpc::Call {
            id: None,
            method: Cow::Borrowed(method),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    #[allow(dead_code)]
    fn result<P>(id: u64, result: P) -> Self
    where
        P: Serialize,
    {
        RawJsonRpc::Result {
            id,
            result: serde_json::to_value(result).unwrap(),
        }
    }

    #[allow(dead_code)]
    fn error(id: Option<u64>, error: RpcError) -> Self {
        RawJsonRpc::Error { id, error }
    }

    fn unwrap_into_result(self) -> Result<Json, RpcError> {
        match self {
            RawJsonRpc::Result { result, .. } => Ok(result),
            RawJsonRpc::Error { error, .. } => Err(error),
            _ => panic!("unwrap_into_result() on bad RawJsonRpc: {:?}", self),
        }
    }
}

impl Serialize for RawJsonRpc {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", "2.0")?;
        match &self {
            RawJsonRpc::Call { id, method, params } => {
                if let Some(id) = id {
                    map.serialize_entry("id", &id)?;
                }
                map.serialize_entry("method", method)?;
                map.serialize_entry("params", params)?;
            }
            RawJsonRpc::Result { id, result } => {
                map.serialize_entry("id", &id)?;
                map.serialize_entry("result", result)?;
            }
            RawJsonRpc::Error { id, error } => {
                map.serialize_entry("id", &id)?;
                map.serialize_entry("error", error)?;
            }
        }
        map.end()
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct RpcError {
    code: i64,
    message: String,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_content_length() {
        assert_eq!(37, parse_content_length("Content-Length: 37").unwrap());
    }

    #[test]
    fn test_raw_jsonrpc_deserialize() {
        let json = json!({"jsonrpc": 2.0, "id": 69, "result": "nice"});
        assert_eq!(
            RawJsonRpc::result(69, "nice"),
            serde_json::from_value(json).unwrap()
        );
        let json = json!({
            "jsonrpc": 2.0,
            "id": null,
            "error": {
                "code": -420,
                "message": "not nice"
            }
        });
        assert_eq!(
            RawJsonRpc::error(
                None,
                RpcError {
                    code: -420,
                    message: "not nice".to_string()
                }
            ),
            serde_json::from_value(json).unwrap()
        );
    }

    #[test]
    fn test_raw_jsonrpc_serialize() {
        let test_request = RawJsonRpc::request(427, "hotdogp", ());
        assert_eq!(
            json!({
                "jsonrpc": "2.0",
                "id": 427,
                "method": "hotdogp",
                "params": null,
            }),
            serde_json::to_value(test_request).unwrap()
        );

        let test_error = RawJsonRpc::error(
            Some(69),
            RpcError {
                code: -420,
                message: "not nice".to_string(),
            },
        );
        assert_eq!(
            json!({
                "jsonrpc": "2.0",
                "id": 69,
                "error": {
                    "code": -420,
                    "message": "not nice",
                }
            }),
            serde_json::to_value(test_error).unwrap()
        );
    }
}
