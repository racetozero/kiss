#![cfg(feature = "native")]

use base64::Engine as _;
use kiss_ai::{
    Context, Message, Registry, ResolvedCredential, StopReason, StreamOptions, Transport,
    UserContent, UserMessage,
};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Debug, Serialize, Deserialize)]
struct Probe {
    case: usize,
    provider: String,
    api: String,
    websocket: bool,
    error: String,
}

#[tokio::test]
async fn providers_use_direct_tls() {
    check_providers(false, false, true, None, "auto", false).await;
}

#[tokio::test]
async fn providers_use_tls_interception_proxy_without_alpn() {
    check_providers(true, false, true, None, "auto", false).await;
}

#[tokio::test]
async fn providers_respect_proxy_bypass() {
    check_providers(false, true, true, None, "auto", false).await;
}

#[tokio::test]
async fn providers_reject_untrusted_proxy_certificates() {
    check_providers(true, false, false, None, "auto", false).await;
}

#[tokio::test]
async fn providers_use_tls_interception_proxy_with_http1_alpn() {
    check_providers(true, false, true, Some(b"http/1.1"), "auto", false).await;
}

#[tokio::test]
async fn cursor_uses_http1_through_tls_interception_proxy() {
    for alpn in [None, Some(b"http/1.1".as_slice())] {
        check_providers(true, false, true, alpn, "http1", false).await;
    }
}

#[tokio::test]
async fn cursor_uses_http2_through_tls_interception_proxy_with_h2_alpn() {
    for mode in ["auto", "http2"] {
        check_providers(true, false, true, Some(b"h2"), mode, false).await;
    }
}

#[tokio::test]
async fn cursor_forced_http2_without_proxy_alpn_rejects_before_input() {
    check_providers(true, false, true, None, "http2", false).await;
}

#[tokio::test]
async fn cursor_transport_overrides_and_no_retry_after_input() {
    for mode in ["http1", "http2", "invalid"] {
        check_providers(false, false, true, None, mode, false).await;
    }
    check_providers(true, false, true, Some(b"http/1.1"), "http2", false).await;
    check_providers(false, false, true, None, "auto", true).await;
}

// Independent upstream field numbers check the H2 streaming-body contract.
#[derive(Clone, PartialEq, prost::Message)]
struct CursorInitial {
    #[prost(message, optional, tag = "1")]
    run: Option<CursorRun>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CursorRun {
    #[prost(message, optional, tag = "9")]
    model: Option<CursorModel>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CursorModel {
    #[prost(string, tag = "1")]
    id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CursorReply {
    #[prost(message, optional, tag = "1")]
    update: Option<CursorUpdate>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CursorUpdate {
    #[prost(message, optional, tag = "1")]
    text: Option<CursorText>,
    #[prost(bytes = "vec", optional, tag = "14")]
    ended: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct CursorText {
    #[prost(string, tag = "1")]
    text: String,
}

async fn check_providers(
    proxy: bool,
    bypass: bool,
    trusted: bool,
    proxy_alpn: Option<&[u8]>,
    cursor_transport: &str,
    cursor_disconnect: bool,
) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certificate = rcgen::generate_simple_self_signed(vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "provider-proxy.test".into(),
    ])
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cert_path = directory.path().join("root.pem");
    let trust_certificate = if trusted {
        certificate.cert.clone()
    } else {
        rcgen::generate_simple_self_signed(vec!["unrelated-root.test".into()])
            .unwrap()
            .cert
    };
    std::fs::write(
        &cert_path,
        format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(trust_certificate.der())
        ),
    )
    .unwrap();
    let output_path = directory.path().join("probes.json");
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der())
                .into(),
        )
        .unwrap();
    if !proxy {
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    } else if let Some(protocol) = proxy_alpn {
        config.alpn_protocols = vec![protocol.to_vec()];
        if protocol == b"h2" {
            config.alpn_protocols.push(b"http/1.1".to_vec());
        }
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let received = Arc::new(Mutex::new(BTreeMap::new()));
    let receipts = Arc::clone(&received);
    let cursor_http2 = cursor_transport == "http2"
        || (cursor_transport == "auto" && (!proxy || proxy_alpn == Some(b"h2")));
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let receipts = Arc::clone(&receipts);
            tokio::spawn(async move {
                if proxy {
                    let mut connect = Vec::new();
                    while !connect.ends_with(b"\r\n\r\n") {
                        connect.push(socket.read_u8().await.unwrap());
                        assert!(connect.len() < 8192);
                    }
                    assert!(connect.starts_with(b"CONNECT provider-proxy.test:443 HTTP/1.1\r\n"));
                    socket
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                }
                let Ok(handshake) = tokio_rustls::LazyConfigAcceptor::new(
                    rustls::server::Acceptor::default(),
                    socket,
                )
                .await
                else {
                    return;
                };
                if proxy
                    && !acceptor.config().alpn_protocols.is_empty()
                    && handshake
                        .client_hello()
                        .alpn()
                        .is_some_and(|mut protocols| {
                            !protocols.any(|protocol| {
                                acceptor
                                    .config()
                                    .alpn_protocols
                                    .iter()
                                    .any(|supported| supported == protocol)
                            })
                        })
                {
                    // Some TLS interception proxies close TCP without a TLS alert
                    // when the client does not offer an accepted ALPN protocol.
                    return;
                }
                let Ok(socket) = handshake.into_stream(Arc::clone(acceptor.config())).await else {
                    return;
                };
                // Read the wire preface because this fixture serves both HTTP versions.
                let mut socket = tokio::io::BufReader::new(socket);
                // A forced H2 request can stop after TLS selects H1, before
                // any application bytes are sent.
                let Ok(preface) = socket.fill_buf().await else {
                    return;
                };
                if preface.is_empty() {
                    return;
                }
                let http2 = preface.starts_with(b"PRI * HTTP/2.0");
                let body =
                    br#"{"message":"KISS_PROXY_REACHED","error":{"message":"KISS_PROXY_REACHED"}}"#;
                if http2 {
                    let mut connection = h2::server::handshake(socket).await.unwrap();
                    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::unbounded_channel();
                    while let Some(request) = tokio::select! {
                        request = connection.accept() => request,
                        _ = disconnect_rx.recv() => None,
                    } {
                        let (request, mut response) = request.unwrap();
                        record_receipt(
                            request.headers(),
                            request.uri().path(),
                            true,
                            &receipts,
                            cursor_http2,
                        );
                        if request.uri().path().ends_with("/agent.v1.AgentService/Run") {
                            let disconnect_tx = disconnect_tx.clone();
                            tokio::spawn(async move {
                                let mut input = request.into_body();
                                let mut bytes = Vec::new();
                                loop {
                                    let chunk = input.data().await.unwrap().unwrap();
                                    input.flow_control().release_capacity(chunk.len()).unwrap();
                                    bytes.extend_from_slice(&chunk);
                                    assert!(bytes.len() < 65536);
                                    if bytes.len() >= 5 {
                                        let length =
                                            u32::from_be_bytes(bytes[1..5].try_into().unwrap())
                                                as usize;
                                        if bytes.len() >= 5 + length {
                                            assert_eq!(bytes[0], 0);
                                            let initial =
                                                CursorInitial::decode(&bytes[5..5 + length])
                                                    .unwrap();
                                            assert!(
                                                initial
                                                    .run
                                                    .unwrap()
                                                    .model
                                                    .unwrap()
                                                    .id
                                                    .starts_with("proxy-case-")
                                            );
                                            break;
                                        }
                                    }
                                }
                                if cursor_disconnect {
                                    // Close only after the server receives the initial prompt.
                                    // Any RunSSE/BidiAppend retry must fail the receipt checks.
                                    disconnect_tx.send(()).unwrap();
                                    return;
                                }
                                let response_headers = http::Response::builder()
                                    .status(200)
                                    .header("content-type", "application/connect+proto")
                                    .body(())
                                    .unwrap();
                                let mut output =
                                    response.send_response(response_headers, false).unwrap();
                                for update in [
                                    CursorUpdate {
                                        text: Some(CursorText {
                                            text: "KISS_PROXY_REACHED".into(),
                                        }),
                                        ended: None,
                                    },
                                    CursorUpdate {
                                        text: None,
                                        ended: Some(Vec::new()),
                                    },
                                ] {
                                    let payload = CursorReply {
                                        update: Some(update),
                                    }
                                    .encode_to_vec();
                                    let frame = [
                                        vec![0],
                                        (payload.len() as u32).to_be_bytes().to_vec(),
                                        payload,
                                    ]
                                    .concat();
                                    output.send_data(frame.into(), false).unwrap();
                                }
                                output.send_data(Vec::new().into(), true).unwrap();
                            });
                        } else {
                            let response_headers = http::Response::builder()
                                .status(400)
                                .header("content-type", "application/json")
                                .header("x-amzn-errortype", "ValidationException")
                                .body(())
                                .unwrap();
                            response
                                .send_response(response_headers, false)
                                .unwrap()
                                .send_data(body.to_vec().into(), true)
                                .unwrap();
                        }
                    }
                } else {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.unwrap());
                        assert!(request.len() < 65536);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let mut headers = http::HeaderMap::new();
                    for line in request.lines().skip(1).filter(|line| !line.is_empty()) {
                        let (name, value) = line.split_once(':').unwrap();
                        headers.insert(
                            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                            http::HeaderValue::from_str(value.trim()).unwrap(),
                        );
                    }
                    record_receipt(
                        &headers,
                        request.lines().next().unwrap(),
                        false,
                        &receipts,
                        cursor_http2,
                    );
                    if headers.contains_key("upgrade") {
                        use futures::SinkExt as _;
                        use tokio_tungstenite::tungstenite::{
                            Message, handshake::derive_accept_key, protocol::Role,
                        };
                        let accept = derive_accept_key(headers["sec-websocket-key"].as_bytes());
                        let response = format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        let mut websocket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                            socket,
                            Role::Server,
                            None,
                        )
                        .await;
                        websocket
                            .send(Message::Text(
                                r#"{"type":"error","error":{"message":"KISS_PROXY_REACHED"}}"#
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        return;
                    }
                    let response = format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nx-amzn-errortype: ValidationException\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    // Cursor can cancel RunSSE when the append request fails.
                    // The parent checks the client's result and request counts.
                    let _ = socket
                        .write_all(&[response.as_bytes(), body].concat())
                        .await;
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .kill_on_drop(true)
        .args(["--exact", "provider_probe", "--ignored", "--nocapture"])
        .env(
            "KISS_PROXY_TEST_URL",
            if proxy {
                "https://provider-proxy.test".into()
            } else {
                format!("https://{address}")
            },
        )
        .env("KISS_PROXY_TEST_OUTPUT", &output_path)
        .env("KISS_CURSOR_TRANSPORT", cursor_transport)
        .env("SSL_CERT_FILE", cert_path)
        .env("SSL_CERT_DIR", directory.path())
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_ACCESS_KEY_ID", "test-access-key")
        .env("AWS_SECRET_ACCESS_KEY", "test-secret-key")
        .env("AWS_REGION", "us-east-1")
        .env("GOOGLE_CLOUD_PROJECT", "test-project")
        .env("GOOGLE_CLOUD_LOCATION", "us-central1");
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "KISS_CURSOR_URL",
    ] {
        command.env_remove(key);
    }
    if proxy || bypass {
        command.env(
            "HTTPS_PROXY",
            if proxy {
                format!("http://{address}")
            } else {
                "http://127.0.0.1:1".into()
            },
        );
    }
    command.env("NO_PROXY", if bypass { "127.0.0.1,localhost" } else { "" });
    let child = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        child.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let probes: Vec<Probe> = serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap();
    let receipts = received.lock().unwrap();
    let failed: Vec<_> = probes
        .iter()
        .filter(|probe| {
            if probe.provider == "cursor" && trusted {
                if cursor_transport == "invalid" {
                    return receipts.keys().any(|(case, _)| *case == probe.case)
                        || !probe.error.contains("Use auto, http1, or http2");
                }
                if cursor_transport == "http2" && proxy && proxy_alpn != Some(b"h2") {
                    return receipts.keys().any(|(case, _)| *case == probe.case)
                        || !probe.error.contains("KISS_CURSOR_TRANSPORT=http1");
                }
                if cursor_http2 && receipts.get(&(probe.case, false)) != Some(&1) {
                    return true;
                }
                if cursor_disconnect {
                    return !probe.error.contains("Cursor connection failed");
                }
            }
            if trusted {
                !receipts.contains_key(&(probe.case, probe.websocket))
                    || (probe.api != "bedrock-converse-stream"
                        && !probe.error.contains("KISS_PROXY_REACHED"))
            } else {
                receipts.keys().any(|(case, _)| *case == probe.case)
                    || probe.error == "request timed out"
            }
        })
        .collect();
    println!(
        "proxy={proxy} bypass={bypass} trusted={trusted}: {} providers, {} provider/API/transport cases, {} received",
        probes
            .iter()
            .map(|probe| &probe.provider)
            .collect::<BTreeSet<_>>()
            .len(),
        probes.len(),
        receipts.len()
    );
    server.abort();
    assert!(
        failed.is_empty(),
        "provider TLS transport checks failed: {failed:#?}"
    );
}

fn record_receipt(
    headers: &http::HeaderMap,
    path: &str,
    http2: bool,
    receipts: &Mutex<BTreeMap<(usize, bool), usize>>,
    cursor_http2: bool,
) {
    if path.contains("/agent.v1.AgentService/GetUsableModels") {
        return;
    }
    let case = headers
        .get("x-kiss-proxy-case")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .or_else(|| {
            path.split("proxy-case-")
                .nth(1)?
                .split(|character: char| !character.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .expect("provider request must contain its test case ID");
    *receipts
        .lock()
        .unwrap()
        .entry((case, headers.contains_key("upgrade")))
        .or_default() += 1;
    if path.contains("/aiserver.v1.BidiService/BidiAppend") {
        assert!(!cursor_http2, "do not retry H2 input through BidiAppend");
    }
    if path.contains("/agent.v1.AgentService/Run") {
        assert!(
            path.contains("/agent.v1.AgentService/RunSSE") != cursor_http2,
            "Cursor must select Run or RunSSE from the negotiated HTTP version"
        );
        assert_eq!(
            http2, cursor_http2,
            "Cursor must use the negotiated HTTP version"
        );
    }
}

#[tokio::test]
#[ignore = "child process used by the TLS transport matrix"]
async fn provider_probe() {
    let endpoint = std::env::var("KISS_PROXY_TEST_URL").expect("run the parent provider TLS tests");
    let mut cases = BTreeMap::new();
    for model in Registry::from_builtin().all() {
        cases
            .entry((model.provider.clone(), model.api.clone()))
            .or_insert_with(|| model.clone());
    }
    let mut probes = Vec::new();
    for mut model in cases.into_values() {
        let websocket_options: &[bool] = if matches!(
            model.api.as_str(),
            "openai-codex-responses" | "azure-openai-responses"
        ) || (model.provider == "openai"
            && model.api == "openai-responses")
        {
            &[false, true]
        } else {
            &[false]
        };
        for &websocket in websocket_options {
            let case = probes.len();
            model.base_url = if model.provider == "cursor" {
                format!("{endpoint}/proxy-case-{case}")
            } else {
                endpoint.clone()
            };
            model.id = format!("proxy-case-{case}");
            model
                .headers
                .insert("x-kiss-proxy-case".into(), case.to_string());
            let token = if model.provider == "openai-codex" {
                format!(
                    "e30.{}.test",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                        br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"test-account"}}"#
                    )
                )
            } else {
                "test-key".into()
            };
            let options = StreamOptions {
                credential: Some(ResolvedCredential::api_key(token)),
                transport: if websocket {
                    Transport::WebSocket
                } else {
                    Transport::Sse
                },
                max_tokens: Some(16),
                ..Default::default()
            };
            let context = Context {
                messages: vec![Message::User(UserMessage {
                    content: UserContent::Text("TLS proxy test".into()),
                    timestamp: 0,
                })],
                ..Default::default()
            };
            let events = kiss_ai::stream_simple(&model, &context, &options);
            let error = match tokio::time::timeout(Duration::from_secs(3), events.result()).await {
                Ok(result) => {
                    if model.provider == "cursor" && result.stop_reason == StopReason::Stop {
                        assert_eq!(result.text(), "KISS_PROXY_REACHED");
                        result.text()
                    } else {
                        assert_eq!(result.stop_reason, StopReason::Error);
                        result.error_message.unwrap_or_default()
                    }
                }
                Err(_) => {
                    options.cancel.cancel();
                    "request timed out".into()
                }
            };
            probes.push(Probe {
                case,
                provider: model.provider.clone(),
                api: model.api.clone(),
                websocket,
                error,
            });
        }
    }
    std::fs::write(
        std::env::var("KISS_PROXY_TEST_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&probes).unwrap(),
    )
    .unwrap();
}
