use crate::{
    bridge_thread::{self, BackendError},
    views::downloads_view::QueuedDownload,
    BackendStallState, GameDetails, GameDetailsWrapper, MaximaEguiApp,
};
use log::{info, warn};
use tokio::sync::mpsc::error::TryRecvError;

const MAX_MESSAGES_PER_FRAME: usize = 64;

pub fn frontend_processor(app: &mut MaximaEguiApp, ctx: &egui::Context) {
    puffin::profile_function!();

    if app.critical_error.is_some() {
        return;
    }

    let mut needs_repaint = false;

    for _ in 0..MAX_MESSAGES_PER_FRAME {
        match app.backend.backend_listener.try_recv() {
            Ok(response) => {
                needs_repaint = true;
                handle_backend_response(app, response);
                if app.critical_error.is_some() {
                    break;
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                app.critical_error = Some(BackendError::ChannelDisconnected);
                break;
            }
        }
    }

    if needs_repaint {
        ctx.request_repaint();
    }
}

fn send_request(app: &mut MaximaEguiApp, request: bridge_thread::MaximaLibRequest) {
    if app.backend.backend_commander.send(request).is_err() {
        warn!("Bridge thread disconnected during send");
        app.critical_error = Some(BackendError::ChannelDisconnected);
    }
}

fn find_slug_for_offer(games: &std::collections::HashMap<String, crate::GameInfo>, offer: &str) -> String {
    games
        .iter()
        .find(|(_, g)| g.offer == offer)
        .map(|(slug, _)| slug.clone())
        .unwrap_or_default()
}

fn handle_backend_response(app: &mut MaximaEguiApp, response: bridge_thread::MaximaLibResponse) {
    use bridge_thread::MaximaLibResponse::*;
    match response {
        LoginResponse(res) => handle_login_response(app, res),
        LoginCacheEmpty => app.backend_state = BackendStallState::UserNeedsToLogIn,
        ServiceNeedsStarting => app.backend_state = BackendStallState::UserNeedsToInstallService,
        ServiceStarted => app.backend_state = BackendStallState::Starting,
        GameInfoResponse(res) => {
            app.games.entry(res.game.slug.clone()).or_insert(res.game);
        }
        GameDetailsResponse(res) => handle_game_details_response(app, res),
        FriendInfoResponse(res) => {
            if !app.friends.iter().any(|f| f.id == res.friend.id) {
                app.friends.push(res.friend);
            }
        }
        CriticalError(err) => app.critical_error = Some(*err),
        NonFatalError(err) => app.nonfatal_errors.push(*err),
        ActiveGameChanged(slug) => app.playing_game = slug,
        LocateGameResponse(res) => {
            app.installer_state.locate_response = Some(res);
            app.installer_state.locating = false;
        }
        DownloadProgressChanged(offer_id, progress) => {
            if let Some(dl) = app.installing_now.as_mut() {
                if dl.offer == offer_id {
                    dl.downloaded_bytes = progress.bytes;
                    dl.total_bytes = progress.bytes_total;
                }
            }
        }
        DownloadFinished(offer_id) => {
            if app.installing_now.as_ref().is_some_and(|n| n.offer == offer_id) {
                app.installing_now = None;
            }
            send_request(app, bridge_thread::MaximaLibRequest::GetGamesRequest);
        }
        DownloadQueueUpdate(current, queue) => handle_download_queue_update(app, current, queue),
    }
}

fn handle_login_response(
    app: &mut MaximaEguiApp,
    res: Result<bridge_thread::InteractThreadLoginResponse, anyhow::Error>,
) {
    match res {
        Err(error) => {
            warn!("Login failed: {}", error);
            app.backend_state = BackendStallState::UserNeedsToLogIn;
        }
        Ok(res) => {
            info!("Logged in as {}!", res.you.display_name());
            app.user_name = res.you.display_name().clone();
            app.user_id = res.you.id().clone();
            app.backend_state = BackendStallState::BingChilling;
            send_request(app, bridge_thread::MaximaLibRequest::GetGamesRequest);
            send_request(app, bridge_thread::MaximaLibRequest::GetFriendsRequest);
        }
    }
}

fn handle_game_details_response(
    app: &mut MaximaEguiApp,
    res: bridge_thread::InteractThreadGameDetailsResponse,
) {
    if let Some(game) = app.games.get_mut(&res.slug) {
        let r = res.response;
        game.details = GameDetailsWrapper::Available(GameDetails {
            time: r.time,
            achievements_unlocked: r.achievements_unlocked,
            achievements_total: r.achievements_total,
            path: r.path.clone(),
            system_requirements_min: r.system_requirements_min.clone(),
            system_requirements_rec: r.system_requirements_rec.clone(),
        });
    }
}

fn handle_download_queue_update(
    app: &mut MaximaEguiApp,
    current: Option<String>,
    queue: Vec<String>,
) {
    if let Some(current_offer) = current {
        if !app.installing_now.as_ref().is_some_and(|n| n.offer == current_offer) {
            let slug = find_slug_for_offer(&app.games, &current_offer);
            app.installing_now = Some(QueuedDownload {
                slug,
                offer: current_offer,
                downloaded_bytes: 0,
                total_bytes: 0,
            });
        }
    } else {
        app.installing_now = None;
    }

    app.install_queue.clear();
    for offer in queue {
        let slug = find_slug_for_offer(&app.games, &offer);
        app.install_queue.insert(
            offer.clone(),
            QueuedDownload { slug, offer, downloaded_bytes: 0, total_bytes: 0 },
        );
    }
}