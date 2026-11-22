//! Async daemon accept loop, control RPC dispatcher, duplex stream handlers, and idle exit monitor.

#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use term_proto::{
    ClientMsg, ControlRequest, ControlResponse, LegacyUpMsg, PROTOCOL_VERSION, ServerMsg,
    TERMD_CONTROL_VERSION, TermStreamDown,
};
use tokio::sync::Notify;

use crate::handover::{HANDOVER_SUPPORTED, send_sessions_over_handover};
use crate::session::{LegacyBroadcast, Session, SessionManager};
#[cfg(unix)]
use crate::transport::connect;
use crate::transport::{BoxedIo, IpcListener, read_ipc_frame, write_ipc_frame};

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub endpoint: String,
    pub parent_pid: Option<u32>,
    pub idle_exit_secs: u64,
    pub takeover: bool,
}

pub async fn run_daemon(opts: DaemonOptions) -> Result<()> {
    let mgr = SessionManager::new();
    let shutdown_notify = Arc::new(Notify::new());
    let skip_unlink = Arc::new(AtomicBool::new(false));

    // If `--takeover` was passed and a previous `termd` instance is reachable on `opts.endpoint`,
    // perform seamless FD + state handover on Unix and swap listener atomically onto `opts.endpoint`.
    #[cfg(unix)]
    let listener = if opts.takeover {
        start_with_unix_takeover(&mgr, &opts.endpoint).await?
    } else {
        IpcListener::bind(&opts.endpoint)?
    };
    #[cfg(not(unix))]
    let listener = {
        let _ = opts.takeover;
        IpcListener::bind(&opts.endpoint)?
    };

    // Idle shutdown monitor (§4.1): exit cleanly when 0 active sessions AND parent process is gone
    // (when parent_pid + non-zero idle_exit_secs configured).
    if opts.idle_exit_secs > 0 {
        let mgr_idle = mgr.clone();
        let shut_idle = Arc::clone(&shutdown_notify);
        let parent_pid = opts.parent_pid;
        let idle_limit = Duration::from_secs(opts.idle_exit_secs);
        tokio::spawn(async move {
            let mut empty_since: Option<tokio::time::Instant> = None;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let count = mgr_idle.len().await;
                if count > 0 {
                    empty_since = None;
                    continue;
                }
                let parent_gone = parent_pid.is_none_or(|p| !is_parent_alive(p));
                if !parent_gone {
                    empty_since = None;
                    continue;
                }
                let since = empty_since.get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed() >= idle_limit {
                    shut_idle.notify_waiters();
                    break;
                }
            }
        });
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok(io) => {
                        let mgr_c = mgr.clone();
                        let shut_c = Arc::clone(&shutdown_notify);
                        let skip_c = Arc::clone(&skip_unlink);
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(io, mgr_c, shut_c, skip_c).await {
                                tracing::debug!("ipc connection closed: {err:#}");
                            }
                        });
                    }
                    Err(err) => {
                        tracing::warn!("accept failed: {err:#}");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            _ = shutdown_notify.notified() => {
                break;
            }
        }
    }

    #[cfg(unix)]
    if !skip_unlink.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(Path::new(&opts.endpoint));
    }

    Ok(())
}

#[cfg(unix)]
async fn start_with_unix_takeover(mgr: &SessionManager, endpoint: &str) -> Result<IpcListener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener as StdUnixListener;

    let endpoint_path = Path::new(endpoint);
    if endpoint_path.exists() && connect(endpoint).await.is_ok() {
        let pid = std::process::id();
        let takeover_sock = format!("{endpoint}.takeover.{pid}");
        let new_sock = format!("{endpoint}.new.{pid}");

        let _ = std::fs::remove_file(&takeover_sock);
        let takeover_listener = StdUnixListener::bind(&takeover_sock)
            .with_context(|| format!("bind takeover socket {takeover_sock}"))?;
        let _ = std::fs::set_permissions(&takeover_sock, std::fs::Permissions::from_mode(0o600));

        let mut old_conn = connect(endpoint).await?;
        let req_bytes = bincode::serialize(&ControlRequest::BeginHandover {
            takeover_socket: takeover_sock.clone(),
        })?;
        write_ipc_frame(&mut old_conn, &req_bytes).await?;

        let recv_task = tokio::spawn({
            let mgr_c = mgr.clone();
            async move { crate::handover::receive_sessions_from_handover(&mgr_c, takeover_listener).await }
        });

        let ack_raw = read_ipc_frame(&mut old_conn)
            .await?
            .context("EOF waiting BeginHandover response")?;
        let resp: ControlResponse = bincode::deserialize(&ack_raw)?;
        if let ControlResponse::Error { code, message } = resp {
            let _ = std::fs::remove_file(&takeover_sock);
            anyhow::bail!("old termd refused handover ({code}): {message}");
        }
        let received = recv_task.await??;
        let _ = std::fs::remove_file(&takeover_sock);
        tracing::info!("received {received} sessions via SCM_RIGHTS handover");

        // Bind new listener at temp path then atomically replace `endpoint` so clients never see a
        // missing socket file.
        let listener = IpcListener::bind(&new_sock)?;
        std::fs::rename(&new_sock, endpoint)
            .with_context(|| format!("atomic swap {new_sock} -> {endpoint}"))?;
        return Ok(listener);
    }

    IpcListener::bind(endpoint)
}

async fn handle_connection(
    mut io: BoxedIo,
    mgr: SessionManager,
    shutdown: Arc<Notify>,
    skip_unlink: Arc<AtomicBool>,
) -> Result<()> {
    while let Some(frame) = read_ipc_frame(&mut io).await? {
        let req: ControlRequest =
            bincode::deserialize(&frame).context("decode ControlRequest")?;

        match req {
            ControlRequest::OpenTermStream { id } => {
                let Some(session) = mgr.get(&id).await else {
                    send_resp(
                        &mut io,
                        &ControlResponse::Error {
                            code: "not_found".into(),
                            message: format!("session {id} not found"),
                        },
                    )
                    .await?;
                    return Ok(());
                };
                send_resp(&mut io, &ControlResponse::StreamReady).await?;
                return run_term_stream(io, session).await;
            }
            ControlRequest::OpenLegacyStream {
                id,
                cols,
                rows,
                client_id,
            } => {
                let Some(session) = mgr.get(&id).await else {
                    send_resp(
                        &mut io,
                        &ControlResponse::Error {
                            code: "not_found".into(),
                            message: format!("session {id} not found"),
                        },
                    )
                    .await?;
                    return Ok(());
                };
                send_resp(&mut io, &ControlResponse::StreamReady).await?;
                return run_legacy_stream(io, session, client_id, cols, rows).await;
            }
            ControlRequest::BeginHandover { takeover_socket } => {
                if !HANDOVER_SUPPORTED {
                    send_resp(
                        &mut io,
                        &ControlResponse::Error {
                            code: "unsupported".into(),
                            message: "handover unsupported on this platform".into(),
                        },
                    )
                    .await?;
                    continue;
                }
                // Respond Ok first so receiver enters accept cleanly, then transfer all sessions.
                send_resp(&mut io, &ControlResponse::Ok { changed: true }).await?;
                match send_sessions_over_handover(&mgr, &takeover_socket).await {
                    Ok(()) => {
                        skip_unlink.store(true, Ordering::Release);
                        shutdown.notify_waiters();
                        return Ok(());
                    }
                    Err(err) => {
                        *mgr.handover_lock.lock().unwrap() = false;
                        tracing::error!("handover send failed: {err:#}");
                        return Err(err);
                    }
                }
            }
            other => {
                let resp = dispatch_control(&mgr, &shutdown, other).await;
                send_resp(&mut io, &resp).await?;
            }
        }
    }
    Ok(())
}

async fn dispatch_control(
    mgr: &SessionManager,
    shutdown: &Notify,
    req: ControlRequest,
) -> ControlResponse {
    match req {
        ControlRequest::Ping => ControlResponse::Pong {
            control_version: TERMD_CONTROL_VERSION,
            protocol_version: PROTOCOL_VERSION.into(),
            binary_version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            session_count: mgr.len().await,
            handover_supported: HANDOVER_SUPPORTED,
        },
        ControlRequest::ShutdownIfIdle => {
            if mgr.len().await == 0 {
                shutdown.notify_waiters();
                ControlResponse::Ok { changed: true }
            } else {
                ControlResponse::Ok { changed: false }
            }
        }
        ControlRequest::CreateSession(c) => match mgr.create_or_ensure(c).await {
            Ok(info) => ControlResponse::Session(info),
            Err(e) => err_resp("create_failed", e),
        },
        ControlRequest::StopSession { id } => {
            let changed = mgr.stop_session(&id).await;
            ControlResponse::Ok { changed }
        }
        ControlRequest::ListSessions => ControlResponse::Sessions(mgr.list_sessions().await),
        ControlRequest::GetPane { id } => ControlResponse::Pane(mgr.get_pane(&id).await),
        ControlRequest::PanePids { id } => ControlResponse::Pids(mgr.pane_pids(&id).await),
        ControlRequest::SendKeys { id, keys } => match mgr.send_keys(&id, &keys).await {
            Ok(changed) => ControlResponse::Ok { changed },
            Err(e) => err_resp("send_keys_failed", e),
        },
        ControlRequest::SendLiteral { id, data } => match mgr.send_literal(&id, &data).await {
            Ok(changed) => ControlResponse::Ok { changed },
            Err(e) => err_resp("send_literal_failed", e),
        },
        ControlRequest::Paste { id, data } => match mgr.paste(&id, &data).await {
            Ok(changed) => ControlResponse::Ok { changed },
            Err(e) => err_resp("paste_failed", e),
        },
        ControlRequest::Resize { id, cols, rows } => match mgr.resize(&id, cols, rows).await {
            Ok(changed) => ControlResponse::Ok { changed },
            Err(e) => err_resp("resize_failed", e),
        },
        ControlRequest::Capture { id, mode } => match mgr.capture(&id, mode).await {
            Ok(txt) => ControlResponse::Text(txt),
            Err(e) => err_resp("capture_failed", e),
        },
        ControlRequest::History {
            id,
            epoch,
            start_seq,
            count,
            tail_lines,
        } => match mgr.history(&id, epoch, start_seq, count, tail_lines).await {
            Ok((slice, epoch_mismatch)) => ControlResponse::History {
                slice,
                epoch_mismatch,
            },
            Err(e) => err_resp("history_failed", e),
        },
        ControlRequest::OutputActivity => ControlResponse::Activity(mgr.output_activity().await),
        ControlRequest::ListOrphans { known_ids } => {
            ControlResponse::Ids(mgr.list_orphans(&known_ids).await)
        }
        ControlRequest::SweepOrphans { keep_ids } => {
            ControlResponse::Ids(mgr.sweep_orphans(&keep_ids).await)
        }
        ControlRequest::BeginHandover { .. }
        | ControlRequest::OpenTermStream { .. }
        | ControlRequest::OpenLegacyStream { .. } => ControlResponse::Error {
            code: "internal".into(),
            message: "unreachable".into(),
        },
    }
}

async fn send_resp(io: &mut BoxedIo, resp: &ControlResponse) -> Result<()> {
    let bytes = bincode::serialize(resp)?;
    write_ipc_frame(io, &bytes).await
}

fn err_resp(code: &str, err: anyhow::Error) -> ControlResponse {
    ControlResponse::Error {
        code: code.into(),
        message: format!("{err:#}"),
    }
}

async fn run_term_stream(mut io: BoxedIo, session: Arc<Session>) -> Result<()> {
    let Some(hello_bytes) = read_ipc_frame(&mut io).await? else {
        return Ok(());
    };
    let hello: ClientMsg = term_proto::decode_client_msg(&hello_bytes)
        .or_else(|_| bincode::deserialize(&hello_bytes).map_err(|e| anyhow::anyhow!("{e}")))?;

    let mut att = match session.attach_viewer(hello).await {
        Ok(att) => att,
        Err(err) => {
            if let Ok(mut enc) = term_proto::ZstdStreamEncoder::new() {
                if let Ok(z) = enc.encode_server_msg(&ServerMsg::Error {
                    code: "hello_rejected".into(),
                    message: err,
                }) {
                    let pkt = bincode::serialize(&TermStreamDown::ZstdChunk(z))?;
                    let _ = write_ipc_frame(&mut io, &pkt).await;
                }
            }
            return Ok(());
        }
    };

    // Emit initial Welcome message on the stream's zstd context.
    let welcome = ServerMsg::Welcome {
        protocol: format!("{PROTOCOL_VERSION} {}", term_proto::CAP_TRZSZ),
        session_id: session.id.clone(),
        client_id: att.client_id.clone(),
    };
    let welcome_z = att.zstd.encode_server_msg(&welcome)?;
    let welcome_pkt = bincode::serialize(&TermStreamDown::ZstdChunk(welcome_z))?;
    write_ipc_frame(&mut io, &welcome_pkt).await?;

    loop {
        let (chunks, retry_after) = session.step_viewer_output(&mut att).await?;
        for z in chunks {
            let down = TermStreamDown::ZstdChunk(z);
            let pkt = bincode::serialize(&down)?;
            write_ipc_frame(&mut io, &pkt).await?;
        }
        let mut pace_sleep = retry_after.map(|d| Box::pin(tokio::time::sleep(d)));

        tokio::select! {
            msg_res = read_ipc_frame(&mut io) => {
                match msg_res {
                    Ok(Some(raw)) => {
                        match term_proto::decode_client_msg(&raw)
                            .or_else(|_| bincode::deserialize(&raw).map_err(|e| anyhow::anyhow!("{e}")))
                        {
                            Ok(cmsg) => {
                                if let Ok(Some(reply)) = session.handle_viewer_msg(&mut att, cmsg).await {
                                    let z = att.zstd.encode_server_msg(&reply)?;
                                    let pkt = bincode::serialize(&TermStreamDown::ZstdChunk(z))?;
                                    if write_ipc_frame(&mut io, &pkt).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            _ = att.wake.notified() => {}
            _ = async {
                match &mut pace_sleep {
                    Some(s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
    }

    session.detach_viewer(att).await;
    Ok(())
}

async fn run_legacy_stream(
    mut io: BoxedIo,
    session: Arc<Session>,
    client_id: String,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let conn_key = format!("legacy-{client_id}-{}", rand_suffix());
    let (init, mut rx) = session.subscribe_legacy(&conn_key, client_id, cols, rows).await;
    write_ipc_frame(&mut io, &bincode::serialize(&init)?).await?;

    loop {
        tokio::select! {
            frame_res = read_ipc_frame(&mut io) => {
                match frame_res? {
                    Some(raw) => {
                        let up: LegacyUpMsg = bincode::deserialize(&raw)?;
                        match up {
                            LegacyUpMsg::Input(bytes) => {
                                let guard = session.state.lock().await;
                                let _ = guard.pty.write_all(&bytes);
                                drop(guard);
                                session.note_legacy_input(&conn_key).await;
                            }
                            LegacyUpMsg::Resize { cols, rows } => {
                                session.note_legacy_resize(&conn_key, cols, rows).await;
                            }
                            LegacyUpMsg::NoteActivity => {
                                session.note_legacy_input(&conn_key).await;
                            }
                        }
                    }
                    None => break,
                }
            }
            b = rx.recv() => {
                match b {
                    Ok(LegacyBroadcast::Output(bytes)) => {
                        let down = term_proto::LegacyDownMsg::Output(bytes);
                        write_ipc_frame(&mut io, &bincode::serialize(&down)?).await?;
                    }
                    Ok(LegacyBroadcast::Title(title)) => {
                        let down = term_proto::LegacyDownMsg::Title(title);
                        write_ipc_frame(&mut io, &bincode::serialize(&down)?).await?;
                    }
                    Ok(LegacyBroadcast::Exited { exit_code }) => {
                        let down = term_proto::LegacyDownMsg::Exited { exit_code };
                        write_ipc_frame(&mut io, &bincode::serialize(&down)?).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Ignore lag warning; xterm continues from live stream cleanly.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    session.unsubscribe_legacy(&conn_key).await;
    Ok(())
}

fn rand_suffix() -> u64 {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn is_parent_alive(pid: u32) -> bool {
    crate::pty::is_pid_alive(pid)
}
