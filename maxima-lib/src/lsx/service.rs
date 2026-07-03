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
        let mut idx = 0_usize;
        while idx < connections.len() {
            let connection = &mut connections[idx];

            if connection.process_queue().await.is_err() {
                warn!("Failed to process LSX message queue");
            }

            if connection.listen().await.is_err() {
                warn!("LSX connection closed");
                connections.remove(idx);
                maxima
                    .lock()
                    .await
                    .set_lsx_connections(connections.len() as u16);
                continue;
            }

            idx += 1;
        }

        let (socket, addr) = match listener.accept().await {
            Ok(s) => s,
            Err(err) => {
                let kind = err.kind();
                if kind == ErrorKind::WouldBlock {
                    sleep(Duration::from_millis(20)).await;
                    continue;
                }
                return Err(LSXServerError::Io(err));
            }
        };

        info!("New LSX connection: {:?}", addr);

        let conn = Connection::new(maxima.clone(), socket).await;
        if conn.is_err() {
            warn!(
                "Failed to establish LSX connection: {}",
                conn.err().unwrap()
            );
            continue;
        }

        let mut conn = conn.unwrap();
        conn.send_challenge().await?;
        connections.push(conn);

        let mut maxima = maxima.lock().await;
        maxima.set_lsx_connections(connections.len() as u16);
        maxima.set_player_started();
    }
}
