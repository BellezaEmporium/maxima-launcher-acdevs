use egui::Context;
use std::sync::mpsc::Sender;

use crate::{
    GameDetails,
    bridge_thread::{BackendError, InteractThreadGameDetailsResponse, MaximaLibResponse},
    util::markdown::html_to_easymark,
};
use maxima::core::{
    LockedMaxima,
    service_layer::{
        SERVICE_REQUEST_GAMESYSTEMREQUIREMENTS, ServiceGameSystemRequirements,
        ServiceGameSystemRequirementsRequestBuilder,
    },
};

pub async fn game_details_request(
    maxima_arc: LockedMaxima,
    slug: String,
    channel: Sender<MaximaLibResponse>,
    ctx: &Context,
) -> Result<(), BackendError> {
    let maxima = maxima_arc.lock().await;

    let rq = maxima.service_layer().request(
        SERVICE_REQUEST_GAMESYSTEMREQUIREMENTS,
        ServiceGameSystemRequirementsRequestBuilder::default()
            .slug(slug.clone())
            .locale(maxima.locale().short_str().to_owned())
            .build()
            .unwrap(),
    );
    let rq: ServiceGameSystemRequirements = rq.await?;

    //TODO: parse async

    let (min, rec) = if rq.system_requirements().len() >= 1 {
        (
            Some(html_to_easymark(rq.system_requirements()[0].minimum())),
            Some(html_to_easymark(rq.system_requirements()[0].recommended())),
        )
    } else {
        (None, None)
    };

    let res = MaximaLibResponse::GameDetailsResponse(InteractThreadGameDetailsResponse {
        slug: slug.clone(),
        response: GameDetails {
            time: 0,
            achievements_unlocked: 0,
            achievements_total: 12,
            path: String::new(),
            system_requirements_min: min,
            system_requirements_rec: rec,
        },
    });
    let _ = channel.send(res);
    egui::Context::request_repaint(&ctx);
    Ok(())
}
