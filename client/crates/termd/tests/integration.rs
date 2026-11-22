#![cfg(unix)]

use std::time::{Duration, Instant};

use anyhow::Result;
use term_proto::{
    CaptureMode, CellSpacer, ClientMsg, ClientScreen, ControlRequest, ControlResponse,
    CreateSessionReq, FEATURE_TRZSZ, PROTOCOL_VERSION, ScreenSnapshot, ServerMsg, TermStreamDown, ViewState,
    ZstdStreamDecoder, encode_client_msg,
};
use termd::transport::{BoxedIo, connect, read_ipc_frame, write_ipc_frame};

async fn rpc(io: &mut BoxedIo, req: &ControlRequest) -> Result<ControlResponse> {
    let bytes = bincode::serialize(req)?;
    write_ipc_frame(io, &bytes).await?;
    let raw = read_ipc_frame(io)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unexpected EOF from termd"))?;
    Ok(bincode::deserialize(&raw)?)
}

fn snap_plain_rows(snap: &ScreenSnapshot) -> Vec<String> {
    snap.lines
        .iter()
        .map(|row| {
            let mut s = String::new();
            for cell in &row.cells {
                if cell.spacer != CellSpacer::None {
                    continue;
                }
                s.push(cell.c);
                s.push_str(&cell.extra);
            }
            s.trim_end().to_owned()
        })
        .collect()
}

#[cfg(unix)]
async fn wait_for_text(endpoint: &str, id: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut io) = connect(endpoint).await {
            if let Ok(ControlResponse::Text(Some(txt))) = rpc(
                &mut io,
                &ControlRequest::Capture {
                    id: id.into(),
                    mode: CaptureMode::PlainText {
                        include_scrollback: true,
                    },
                },
            )
            .await
            {
                if txt.contains(needle) {
                    return txt;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for '{needle}' in session {id}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[cfg(unix)]
async fn wait_for_ping(endpoint: &str) -> ControlResponse {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut io) = connect(endpoint).await {
            if let Ok(resp @ ControlResponse::Pong { .. }) = rpc(&mut io, &ControlRequest::Ping).await
            {
                return resp;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for termd at {endpoint}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_session_queries_stream_pacing_resize_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("termd.sock");
    let sock_str = sock.to_string_lossy().into_owned();

    let exe = env!("CARGO_BIN_EXE_looklook-termd");
    let mut daemon = std::process::Command::new(exe)
        .args(["--socket", &sock_str, "--no-idle-exit"])
        .spawn()
        .expect("spawn looklook-termd");

    let pong = wait_for_ping(&sock_str).await;
    match pong {
        ControlResponse::Pong {
            protocol_version,
            session_count,
            handover_supported,
            ..
        } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert_eq!(session_count, 0);
            assert!(handover_supported);
        }
        other => panic!("unexpected pong: {other:?}"),
    }

    let mut io = connect(&sock_str).await.unwrap();
    let created = rpc(
        &mut io,
        &ControlRequest::CreateSession(CreateSessionReq {
            id: "s1".into(),
            cmd: "/bin/sh".into(),
            args: Vec::new(),
            cwd: dir.path().to_string_lossy().into_owned(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cols: 80,
            rows: 24,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(created, ControlResponse::Session(_)));

    // Open a `looklook-term.v1` stream viewer, test Ack pacing + Delta reconstruction.
    let mut stream_io = connect(&sock_str).await.unwrap();
    let ready = rpc(
        &mut stream_io,
        &ControlRequest::OpenTermStream { id: "s1".into() },
    )
    .await
    .unwrap();
    assert!(matches!(ready, ControlResponse::StreamReady));

    let hello = encode_client_msg(&ClientMsg::Hello {
        protocol: PROTOCOL_VERSION.into(),
        cols: 80,
        rows: 24,
        client_id: Some("viewer-a".into()),
        last_frame: None,
        last_input_seq: 0,
    })
    .unwrap();
    write_ipc_frame(&mut stream_io, &hello).await.unwrap();

    let mut zdec = ZstdStreamDecoder::new().unwrap();
    let mut client_screen = ClientScreen::new();

    // Send input with seq=1 and duplicate seq=1 (must be ignored by dedup!).
    let cmd_in = encode_client_msg(&ClientMsg::Input {
        seq: 1,
        data: b"printf 'HEX=%s\\n' __STREAM_PROBE__\n".to_vec(),
    })
    .unwrap();
    write_ipc_frame(&mut stream_io, &cmd_in).await.unwrap();
    write_ipc_frame(&mut stream_io, &cmd_in).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_welcome = false;
    let mut saw_probe = false;
    while Instant::now() < deadline {
        let pkt_raw = tokio::time::timeout(Duration::from_millis(500), read_ipc_frame(&mut stream_io))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        let Some(pkt_raw) = pkt_raw else {
            continue;
        };
        let TermStreamDown::ZstdChunk(chunk) = bincode::deserialize(&pkt_raw).unwrap() else {
            panic!("unexpected close");
        };
        for msg in zdec.feed(&chunk).unwrap() {
            match &msg {
                ServerMsg::Welcome { client_id, .. } => {
                    assert_eq!(client_id, "viewer-a");
                    saw_welcome = true;
                }
                ServerMsg::Snapshot { frame, .. } => {
                    client_screen.apply_server_msg(&msg).unwrap();
                    let ack = encode_client_msg(&ClientMsg::Ack { frame: *frame }).unwrap();
                    write_ipc_frame(&mut stream_io, &ack).await.unwrap();
                }
                ServerMsg::Delta(d) => {
                    client_screen.apply_server_msg(&msg).unwrap();
                    let ack = encode_client_msg(&ClientMsg::Ack { frame: d.frame }).unwrap();
                    write_ipc_frame(&mut stream_io, &ack).await.unwrap();
                }
                _ => {}
            }
        }
        if let Some(snap) = client_screen.snapshot() {
            let joined = snap_plain_rows(snap).join("\n");
            let count = joined.matches("HEX=__STREAM_PROBE__").count();
            if count == 1 {
                saw_probe = true;
                break;
            }
        }
    }
    assert!(saw_welcome);
    assert!(saw_probe, "viewer must reconstruct command output via Snapshot+Delta with input dedup");

    // Test disconnect & resume (Hello with last_frame & client_id).
    let resume_frame = client_screen.frame();
    drop(stream_io);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify tmux-replacement RPC queries (§4.6).
    let _ = rpc(
        &mut io,
        &ControlRequest::SendLiteral {
            id: "s1".into(),
            data: "printf '\\033]2;custom_title\\007\\n'".into(),
        },
    )
    .await
    .unwrap();
    let _ = rpc(
        &mut io,
        &ControlRequest::SendKeys {
            id: "s1".into(),
            keys: vec!["Enter".into()],
        },
    )
    .await
    .unwrap();

    // Small resize below 20x5 must be ignored by resize policy.
    let mut resume_io = connect(&sock_str).await.unwrap();
    assert!(matches!(
        rpc(
            &mut resume_io,
            &ControlRequest::OpenTermStream { id: "s1".into() }
        )
        .await
        .unwrap(),
        ControlResponse::StreamReady
    ));
    let hello_resume = encode_client_msg(&ClientMsg::Hello {
        protocol: PROTOCOL_VERSION.into(),
        cols: 10,
        rows: 3,
        client_id: Some("viewer-a".into()),
        last_frame: Some(resume_frame),
        last_input_seq: 1,
    })
    .unwrap();
    write_ipc_frame(&mut resume_io, &hello_resume).await.unwrap();
    // Send valid View Hidden & Background toggle cleanly.
    write_ipc_frame(
        &mut resume_io,
        &encode_client_msg(&ClientMsg::View {
            state: ViewState::Background,
        })
        .unwrap(),
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let pane = rpc(
        &mut io,
        &ControlRequest::GetPane { id: "s1".into() },
    )
    .await
    .unwrap();
    match pane {
        ControlResponse::Pane(Some(p)) => {
            assert!(p.alive);
            assert!(p.pid > 0);
            assert_eq!((p.cols, p.rows), (80, 24), "tiny 10x3 attach must not shrink PTY");
        }
        other => panic!("unexpected pane response: {other:?}"),
    }

    let pids = rpc(&mut io, &ControlRequest::PanePids { id: "s1".into() })
        .await
        .unwrap();
    assert!(matches!(pids, ControlResponse::Pids(ref v) if !v.is_empty()));

    let orphans = rpc(
        &mut io,
        &ControlRequest::ListOrphans {
            known_ids: vec!["other".into()],
        },
    )
    .await
    .unwrap();
    assert_eq!(orphans, ControlResponse::Ids(vec!["s1".into()]));

    let swept = rpc(
        &mut io,
        &ControlRequest::SweepOrphans {
            keep_ids: Vec::new(),
        },
    )
    .await
    .unwrap();
    assert_eq!(swept, ControlResponse::Ids(vec!["s1".into()]));

    drop(resume_io);
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[cfg(unix)]
#[tokio::test]
async fn unix_upgrade_handover_keeps_shell_and_state_alive() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("handover.sock");
    let sock_str = sock.to_string_lossy().into_owned();
    let exe = env!("CARGO_BIN_EXE_looklook-termd");

    let mut daemon_a = std::process::Command::new(exe)
        .args(["--socket", &sock_str, "--no-idle-exit"])
        .spawn()
        .expect("spawn old termd A");
    let pong_a = wait_for_ping(&sock_str).await;
    let pid_a = match pong_a {
        ControlResponse::Pong { pid, .. } => pid,
        other => panic!("unexpected pong A: {other:?}"),
    };

    let mut io = connect(&sock_str).await.unwrap();
    let created = rpc(
        &mut io,
        &ControlRequest::CreateSession(CreateSessionReq {
            id: "persist-1".into(),
            cmd: "/bin/sh".into(),
            args: Vec::new(),
            cwd: dir.path().to_string_lossy().into_owned(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cols: 80,
            rows: 24,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(created, ControlResponse::Session(_)));

    // Set an in-memory shell variable in daemon A that ONLY exists inside the live child shell.
    rpc(
        &mut io,
        &ControlRequest::SendLiteral {
            id: "persist-1".into(),
            data: "HANDOVER_TOKEN=alive_across_upgrade\n".into(),
        },
    )
    .await
    .unwrap();
    rpc(
        &mut io,
        &ControlRequest::SendLiteral {
            id: "persist-1".into(),
            data: "echo __READY_FOR_UPGRADE__\n".into(),
        },
    )
    .await
    .unwrap();
    wait_for_text(&sock_str, "persist-1", "__READY_FOR_UPGRADE__").await;
    drop(io);

    // Spawn new termd B with `--takeover` against A's socket.
    let mut daemon_b = std::process::Command::new(exe)
        .args(["--socket", &sock_str, "--takeover", "--no-idle-exit"])
        .spawn()
        .expect("spawn new termd B");

    // Wait for daemon A to exit cleanly after handing over FDs + serialized core state.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(status)) = daemon_a.try_wait() {
            assert!(status.success(), "old termd A exited with {status}");
            break;
        }
        assert!(Instant::now() < deadline, "old termd A did not exit after handover");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Verify socket now serves daemon B (`pid != pid_a`) with session `persist-1` intact!
    let pong_b = wait_for_ping(&sock_str).await;
    match pong_b {
        ControlResponse::Pong {
            pid, session_count, ..
        } => {
            assert_ne!(pid, pid_a);
            assert_eq!(session_count, 1);
        }
        other => panic!("unexpected pong B: {other:?}"),
    }

    // Old captured text survived core handover, AND writing a new command proves the shell
    // process kept running across the daemon replacement!
    let mut io_b = connect(&sock_str).await.unwrap();
    let before = rpc(
        &mut io_b,
        &ControlRequest::Capture {
            id: "persist-1".into(),
            mode: CaptureMode::PlainText {
                include_scrollback: true,
            },
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(before, ControlResponse::Text(Some(ref s)) if s.contains("__READY_FOR_UPGRADE__")),
        "previous screen text must survive handover"
    );

    rpc(
        &mut io_b,
        &ControlRequest::SendLiteral {
            id: "persist-1".into(),
            data: "echo TOKEN_NOW=$HANDOVER_TOKEN\n".into(),
        },
    )
    .await
    .unwrap();
    let after = wait_for_text(&sock_str, "persist-1", "TOKEN_NOW=alive_across_upgrade").await;
    assert!(after.contains("TOKEN_NOW=alive_across_upgrade"));

    let _ = rpc(
        &mut io_b,
        &ControlRequest::StopSession {
            id: "persist-1".into(),
        },
    )
    .await;
    let _ = daemon_b.kill();
    let _ = daemon_b.wait();
}

/// trzsz（设计稿 §5.8）：声明了 FEATURE_TRZSZ 的端收到握手起的原始字节（不进内核），done 之后回到正常模式；
/// 帧里的 acked_input_seq 在程序有输出之后才前进（本地预测回显 §5.7 靠它判断）。
#[cfg(unix)]
#[tokio::test]
async fn trzsz_raw_mode_and_echo_ack() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("termd.sock");
    let sock_str = sock.to_string_lossy().into_owned();
    let exe = env!("CARGO_BIN_EXE_looklook-termd");
    let mut daemon = std::process::Command::new(exe)
        .args(["--socket", &sock_str, "--no-idle-exit"])
        .spawn()
        .expect("spawn looklook-termd");
    wait_for_ping(&sock_str).await;

    let mut io = connect(&sock_str).await.unwrap();
    let created = rpc(
        &mut io,
        &ControlRequest::CreateSession(CreateSessionReq {
            id: "t1".into(),
            cmd: "/bin/sh".into(),
            args: Vec::new(),
            cwd: dir.path().to_string_lossy().into_owned(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cols: 80,
            rows: 24,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(created, ControlResponse::Session(_)));

    let mut s = connect(&sock_str).await.unwrap();
    let ready = rpc(&mut s, &ControlRequest::OpenTermStream { id: "t1".into() }).await.unwrap();
    assert!(matches!(ready, ControlResponse::StreamReady));
    let send = |msg: ClientMsg| encode_client_msg(&msg).unwrap();
    write_ipc_frame(
        &mut s,
        &send(ClientMsg::Hello {
            protocol: PROTOCOL_VERSION.into(),
            cols: 80,
            rows: 24,
            client_id: Some("trz".into()),
            last_frame: None,
            last_input_seq: 0,
        }),
    )
    .await
    .unwrap();
    write_ipc_frame(&mut s, &send(ClientMsg::Features { flags: FEATURE_TRZSZ })).await.unwrap();
    // 命令行本身的回显里魔术串被拆开了，只有 printf 的输出是完整的
    write_ipc_frame(
        &mut s,
        &send(ClientMsg::Input {
            seq: 1,
            data: b"printf 'BEFORE\\n::TR''ZSZ:TRANSFER:R:1.1.5:0000000000000\\nSECRET\\n'\n".to_vec(),
        }),
    )
    .await
    .unwrap();

    let mut zdec = ZstdStreamDecoder::new().unwrap();
    let mut screen = ClientScreen::new();
    let mut caps = String::new();
    let mut raw_id = None;
    let mut raw = Vec::new();
    let mut phase = 0;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        assert!(Instant::now() < deadline, "phase {phase} timed out; raw={:?}", String::from_utf8_lossy(&raw));
        let pkt = tokio::time::timeout(Duration::from_millis(300), read_ipc_frame(&mut s))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten();
        if let Some(pkt) = pkt {
            let TermStreamDown::ZstdChunk(chunk) = bincode::deserialize(&pkt).unwrap() else {
                panic!("closed");
            };
            for msg in zdec.feed(&chunk).unwrap() {
                match &msg {
                    ServerMsg::Welcome { protocol, .. } => caps = protocol.clone(),
                    ServerMsg::Snapshot { frame, .. } | ServerMsg::Delta(term_proto::ScreenDelta { frame, .. }) => {
                        let frame = *frame;
                        screen.apply_server_msg(&msg).unwrap();
                        write_ipc_frame(&mut s, &send(ClientMsg::Ack { frame })).await.unwrap();
                    }
                    ServerMsg::Raw { id, data } => {
                        raw_id = Some(*id);
                        raw.extend_from_slice(data);
                    }
                    _ => {}
                }
            }
        }
        let text = screen.snapshot().map(|s| snap_plain_rows(s).join("\n")).unwrap_or_default();
        match phase {
            // 原始字节不受发帧节奏限制，可能比画出 BEFORE 的那一帧先到
            0 if String::from_utf8_lossy(&raw).contains("SECRET") && text.contains("\nBEFORE") => {
                assert!(
                    !text.lines().any(|l| l.starts_with("::TRZSZ") || l == "SECRET"),
                    "raw bytes must not reach the kernel: {text}"
                );
                assert!(raw.starts_with(b"::TRZSZ:TRANSFER:R:"), "{:?}", String::from_utf8_lossy(&raw));
                assert!(screen.acked_input_seq() >= 1);
                // 端说：这一段处理完了，顺带写一行到屏幕上
                write_ipc_frame(
                    &mut s,
                    &send(ClientMsg::Raw {
                        id: raw_id.unwrap(),
                        consumed: raw.len() as u64,
                        write: b"WRITTEN-BY-CLIENT\r\n".to_vec(),
                        done: true,
                    }),
                )
                .await
                .unwrap();
                write_ipc_frame(&mut s, &send(ClientMsg::Input { seq: 2, data: b"echo BACK''TO''NORMAL\n".to_vec() }))
                    .await
                    .unwrap();
                phase = 1;
            }
            1 if text.contains("BACKTONORMAL") => {
                assert!(text.contains("WRITTEN-BY-CLIENT"), "{text}");
                assert!(!text.lines().any(|l| l == "SECRET"), "{text}");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(caps, format!("{PROTOCOL_VERSION} trzsz"));
    // 没有输出的输入：过了宽限期也会确认（端上靠这一帧撤掉没回显的预测）
    write_ipc_frame(&mut s, &send(ClientMsg::Input { seq: 3, data: b"\x1b".to_vec() })).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while screen.acked_input_seq() < 3 {
        assert!(Instant::now() < deadline, "echo ack for silent input");
        let Ok(Ok(Some(pkt))) = tokio::time::timeout(Duration::from_millis(300), read_ipc_frame(&mut s)).await else {
            continue;
        };
        let TermStreamDown::ZstdChunk(chunk) = bincode::deserialize(&pkt).unwrap() else { panic!("closed") };
        for msg in zdec.feed(&chunk).unwrap() {
            if let ServerMsg::Snapshot { frame, .. } | ServerMsg::Delta(term_proto::ScreenDelta { frame, .. }) = &msg {
                let frame = *frame;
                screen.apply_server_msg(&msg).unwrap();
                write_ipc_frame(&mut s, &send(ClientMsg::Ack { frame })).await.unwrap();
            }
        }
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}
