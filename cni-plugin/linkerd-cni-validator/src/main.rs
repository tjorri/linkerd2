use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use clap::Parser;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, error, info, Instrument};

#[derive(Parser)]
#[clap(version)]
struct Args {
    #[clap(
        long,
        env = "LINKERD_VALIDATION_LOG_LEVEL",
        default_value = "linkerd_cni_validator=info,warn"
    )]
    log_level: kubert::LogFilter,

    #[clap(long, default_value = "plain")]
    log_format: kubert::LogFormat,

    #[clap(long, default_value = "0.0.0.0:4140")]
    outbound_proxy_addr: SocketAddr,

    #[clap(parse(try_from_str), long, env = "KUBERNETES_SERVICE_HOST")]
    target_ip: IpAddr,

    #[clap(parse(try_from_str), long, env = "KUBERNETES_SERVICE_PORT")]
    target_port: u16,
}

static REDIRECT_RESPONSE: &str = "REDIRECTION SUCCESSFUL";

// ERRNO 95: Operation not supported
const UNSUCCESSFUL_EXIT_CODE: i32 = 95;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let Args {
        log_level,
        log_format,
        outbound_proxy_addr,
        target_ip,
        target_port,
    } = Args::parse();

    log_format.try_init(log_level)?;
    let target_addr = SocketAddr::new(target_ip, target_port);

    info!(%outbound_proxy_addr, %target_addr, "Validating outbound traffic is redirected to proxy's outbound port");

    let (shutdown_tx, shutdown_rx) = kubert::shutdown::sigint_or_sigterm()?;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

    let server_task = tokio::spawn(outbound_serve(outbound_proxy_addr, ready_tx, shutdown_rx));

    let validation_task = tokio::spawn(validate_outbound_redirect(target_addr, ready_rx));

    tokio::select! {
        task = validation_task => {
            let validation_result = task.expect("Failed to run validator task");
            if let Err(err) = validation_result {
                error!(error = %err, "Failed validation");
                std::process::exit(UNSUCCESSFUL_EXIT_CODE);
            }

            info!("Validation passed successfully...exiting");
            Ok(())
        }

        _ = shutdown_tx.signaled() => {
            if let Err(e) = server_task.await.expect("Failed to run outbound server task") {
                error!(error = %e, "Failed to validate outbound routing configuration");
                std::process::exit(UNSUCCESSFUL_EXIT_CODE);
            }

            error!("Failed to validate due to server terminating early");
            std::process::exit(UNSUCCESSFUL_EXIT_CODE);
        }
    }
}

async fn validate_outbound_redirect(
    target_addr: SocketAddr,
    ready_rx: tokio::sync::oneshot::Receiver<Result<()>>,
) -> Result<()> {
    let timeout = std::time::Duration::from_secs(120);
    if (tokio::time::timeout(timeout, ready_rx).await).is_err() {
        anyhow::bail!("timed-out ({:?}) waiting for server to be ready", timeout);
    }

    let mut stream = {
        debug!(original_dst = %target_addr, "Building validation client");
        let socket = TcpStream::connect(target_addr).await?;
        let peer = socket.peer_addr()?;
        debug!(original_dst = %peer, "Client connected to validation server");
        socket
    };

    tokio::select! {
        is_readable = stream.readable() => {
            if let Err(e) = is_readable {
                anyhow::bail!("cannot read off client socket {}", e);
            }
        }

        () = tokio::time::sleep(timeout) => {
            anyhow::bail!("timed-out ({:?}) waiting for socket to become readable", timeout);
        }
    };

    let mut buf = [0u8; 100];
    let read_sz = stream.read(&mut buf[..REDIRECT_RESPONSE.len()]).await?;
    let resp = String::from_utf8(buf[..REDIRECT_RESPONSE.len()].to_vec())?;
    debug!(redirect_response = %resp, bytes_read = %read_sz);

    if resp == REDIRECT_RESPONSE {
        Ok(())
    } else {
        anyhow::bail!(
            "expected client to receive {:?}, got {:?} instead",
            REDIRECT_RESPONSE,
            resp,
        );
    }
}
#[tracing::instrument(name = "outbound_server", skip(ready_tx, shutdown))]
async fn outbound_serve(
    listen_addr: SocketAddr,
    ready_tx: tokio::sync::oneshot::Sender<Result<()>>,
    shutdown: kubert::shutdown::Watch,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .expect("Failed to bind server");
    info!(%listen_addr, "Listening for incoming connections");

    if ready_tx.send(Ok(())).is_err() {
        error!("Failed to send 'ready' signal, receiver dropped");
        anyhow::bail!("Failed to bind server");
    }

    let resp_bytes = REDIRECT_RESPONSE.as_bytes();
    tokio::spawn(handle_conn(listener, resp_bytes));

    let _handle = shutdown.signaled().await;
    debug!("Received shutdown signal");
    Ok(())
}

#[tracing::instrument(name = "outbound_handler", skip_all)]
async fn handle_conn(listener: TcpListener, resp_bytes: &'static [u8]) {
    loop {
        let (mut stream, client_addr) = listener
            .accept()
            .await
            .expect("Failed to establish connection");
        info!("Accepted connection");
        let _ = tokio::spawn(async move {
            match stream.write_all(resp_bytes).await {
                Ok(()) => debug!(written_bytes = resp_bytes.len()),
                Err(error) => error!(%error, "Failed to write bytes to client"),
            }
        })
        .instrument(tracing::info_span!("conn", %client_addr));
    }
}