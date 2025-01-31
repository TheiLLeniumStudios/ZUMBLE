use crate::error::MumbleError;
use crate::state::{ServerState, ServerStateRef};
use std::sync::Arc;
use std::time::Instant;

pub async fn clean_loop(state: ServerStateRef) {
    loop {
        tracing::trace!("cleaning clients");

        match clean_run(&state).await {
            Ok(_) => (),
            Err(e) => {
                tracing::error!("error in clean loop: {}", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn clean_run(state: &ServerState) -> Result<(), MumbleError> {
    let mut clients_to_remove = Vec::new();
    let mut clients_to_reset_crypt = Vec::new();

    {
        let mut iter = state.clients.first_entry_async().await;
        while let Some(client) = iter {
            // if we can reset our crypt state, we should block resets if we hare being removed or
            // if the publisher is closed
            let mut can_reset_crypt = true;
            if client.publisher.is_closed() {
                can_reset_crypt = false;
                clients_to_remove.push(client.session_id);
            }

            let now = Instant::now();

            let duration = now.duration_since(client.last_ping.load());

            if duration.as_secs() > 30 {
                can_reset_crypt = false;
                clients_to_remove.push(client.session_id);
            }

            if can_reset_crypt {
                let last_good = { client.crypt_state.lock().await.last_good };

                if now.duration_since(last_good).as_millis() > 8000 {
                    clients_to_reset_crypt.push(Arc::clone(client.get()))
                }
            }

            iter = client.next_async().await;
        }
    }

    for client in clients_to_reset_crypt {
        let session_id = client.session_id;
        if let Err(e) = state.reset_client_crypt(&client).await {
            tracing::error!("failed to send crypt setup for {}: {:?}", e, session_id);
        } else {
            tracing::info!("Requesting {} crypt be reset", client);
        }
    }

    for session_id in clients_to_remove {
        state.disconnect(session_id).await;
    }

    Ok(())
}
