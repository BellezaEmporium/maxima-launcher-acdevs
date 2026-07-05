use std::{io::ErrorKind, time::Duration};

use log::{info, warn};
use tokio::{net::TcpListener, time::sleep};

use crate::lsx::connection::LSXConnectionError;
use crate::{core::LockedMaxima, lsx::connection::Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LSXServerError {
    #[error(transparent)]
    Conn(#[from] LSXConnectionError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub async fn start_server(port: u16, maxima: LockedMaxima) -> Result<(), LSXServerError> {
    let addr = "127.0.0.1:".to_string() + port.to_string().as_str();

    let listener = TcpListener::bind(&addr).await?;
    info!("Listening on: {}", addr);

    let mut connections: Vec<Connection> = Vec::new();

    loop {
        let mut idx = 0;
        while idx < connections.len() {
            if connections[idx].process_queue().await.is_err()
                || connections[idx].listen().await.is_err()
            {
                warn!("LSX connection closed");
                connections.remove(idx);
                maxima
                    .lock()
                    .await
                    .set_lsx_connections(connections.len() as u16);
            } else {
                idx += 1;
            }
        }

        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((socket, addr)) => {
                        info!("New LSX connection: {:?}", addr);
                        match Connection::new(maxima.clone(), socket).await {
                            Ok(mut conn) => {
                                conn.send_challenge().await?;
                                connections.push(conn);
                                maxima.lock().await.set_lsx_connections(connections.len() as u16);
                                maxima.lock().await.set_player_started();
                            }
                            Err(e) => warn!("Failed to establish LSX connection: {}", e),
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        sleep(Duration::from_millis(5)).await;
                    }
                    Err(e) => return Err(LSXServerError::Io(e)),
                }
            }
            _ = sleep(Duration::from_millis(5)) => {
                // YOUUUUUUUU SHALL NOT BLOCK !
            }
        }
    }
}
