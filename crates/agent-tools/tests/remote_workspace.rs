use agent_tools::{RemoteSandboxWorkspace, SandboxConfig, WorkspaceBackend};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn backend(listener: &TcpListener) -> RemoteSandboxWorkspace {
    RemoteSandboxWorkspace::new(SandboxConfig {
        endpoint: format!("http://{}", listener.local_addr().unwrap()),
        sandbox_id: "example".into(),
        token: Some("example-token".into()),
        workdir: "/sandbox".into(),
        remote_spawn: false,
    })
}

#[test]
fn remote_exec_sends_the_caller_deadline_without_a_two_minute_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let workspace = backend(&listener);
    let server = std::thread::spawn(move || {
        for timeout_ms in [180_001, 30_000, 50] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), "POST /v1/sandboxes/example/exec HTTP/1.1");
            let mut length = None;
            let mut auth = None;
            loop {
                line.clear();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.trim_end().split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    } else if name.eq_ignore_ascii_case("authorization") {
                        auth = Some(value.trim().to_owned());
                    }
                }
            }
            assert_eq!(auth.as_deref(), Some("Bearer example-token"));
            let mut bytes = vec![0; length.unwrap()];
            reader.read_exact(&mut bytes).unwrap();
            let payload: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                payload,
                json!({"timeout_ms": timeout_ms, "command": "printf done", "cwd": "/sandbox/sub"})
            );
            let body = r#"{"stdout":"done","stderr":"","exit_code":0}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    for timeout in [Some(180_001), None, Some(50)] {
        let res = workspace.exec("printf done", Some("sub"), timeout).unwrap();
        assert_eq!(res.stdout, "done");
        assert_eq!(res.exit_code, 0);
    }
    server.join().unwrap();
}

#[test]
fn remote_exec_rejects_zero_without_contacting_the_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let workspace = backend(&listener);
    let error = workspace.exec("printf done", None, Some(0)).unwrap_err();
    assert!(error.contains("timeout_ms must be positive"));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
