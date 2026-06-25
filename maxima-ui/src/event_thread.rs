use egui::Context;
use tokio::{sync::mpsc::{UnboundedSender, UnboundedReceiver}, time::Duration};

use crate::bridge_thread::BackendError;
use log::{error, info};
use maxima::core::{
    LockedMaxima,
    service_layer::{
        SERVICE_REQUEST_GETMYFRIENDS, ServiceFriends, ServiceGetMyFriendsRequestBuilder,
    },
};

// TODO(headassbtw): integrate this into the enum too (out of scope for the PR i wrote this in)
pub struct EventThreadFriendStatusResponse {
    pub id: String,
    pub presence: maxima::rtm::client::RichPresence,
}

pub enum MaximaEventResponse {
    FriendStatusResponse(EventThreadFriendStatusResponse),
}

pub enum MaximaEventRequest {
    SubscribeToFriendPresence,
    ShutdownRequest,
}

pub struct EventThread {}

impl EventThread {
    pub fn new(
        ctx: &Context,
        maxima: LockedMaxima,
        rtm_cmd_listener: UnboundedReceiver<MaximaEventRequest>,
        rtm_responder: UnboundedSender<MaximaEventResponse>,
    ) -> Self {
        let context = ctx.clone();

        tokio::task::spawn(async move {
            match EventThread::run(rtm_cmd_listener, rtm_responder, &context, maxima).await {
                Ok(()) => info!("Event thread shut down cleanly"),
                Err(e) => error!("Event thread error: {e}"),
            }
        });

        Self {}
    }

    async fn run(
        mut rtm_cmd_listener: UnboundedReceiver<MaximaEventRequest>,
        rtm_responder: UnboundedSender<MaximaEventResponse>,
        ctx: &Context,
        maxima_arc: LockedMaxima,
    ) -> Result<(), BackendError> {
        let mut maxima = maxima_arc.lock().await;

        let friends: ServiceFriends = maxima
            .service_layer()
            .request(
                SERVICE_REQUEST_GETMYFRIENDS,
                ServiceGetMyFriendsRequestBuilder::default()
                    .offset(0)
                    .limit(100)
                    .is_mutual_friends_enabled(false)
                    .build()
                    .unwrap(),
            )
            .await?;

        let rtm = maxima.rtm();
        rtm.login().await?;

        let players: Vec<String> =
            friends.friends().items().iter().map(|f| f.id().to_owned()).collect();
        info!("Subscribed to {} players", players.len());

        rtm.subscribe(&players).await?;
        drop(maxima);

        let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(30));
        heartbeat_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Presence polling interval — separate from heartbeat
        let mut presence_interval = tokio::time::interval(Duration::from_millis(500));
        presence_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = heartbeat_interval.tick() => {
                    let mut maxima = maxima_arc.lock().await;
                    if let Err(e) = maxima.rtm().heartbeat().await {
                        error!("RTM heartbeat failed: {e}");
                    }
                }

                _ = presence_interval.tick() => {
                    let mut maxima = maxima_arc.lock().await;
                    let store = maxima.rtm().presence_store().lock().await;
                    for entry in store.iter() {
                        let _ = rtm_responder
                            .send(MaximaEventResponse::FriendStatusResponse(
                                EventThreadFriendStatusResponse {
                                    id: entry.0.to_string(),
                                    presence: entry.1,
                                },
                            ))
                            .ok();
                    }
                    if store.entry_count() > 0 {
                        ctx.request_repaint();
                    }
                }

                request = rtm_cmd_listener.recv() => {
                    match request {
                        Some(MaximaEventRequest::ShutdownRequest) | None => return Ok(()),
                        Some(MaximaEventRequest::SubscribeToFriendPresence) => {}
                    }
                }
            }
        }
    }
}