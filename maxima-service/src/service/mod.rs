use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use actix_web::{HttpResponse, Responder, get, post, web};
use log::{error, info};
use maxima::util::registry::set_up_registry;
use structured_logger::json::new_writer;
use tokio::sync::oneshot;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::{
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::service::error::ServerError;
use crate::service::hash::get_sha256_hash_of_pid;
use maxima::core::background_service::{BACKGROUND_SERVICE_PORT, ServiceLibraryInjectionRequest};
use maxima::util::dll_injector::{DllInjector, InjectionError};
use maxima::util::native::SafeParent;
use maxima::util::service::SERVICE_NAME;

pub(crate) mod error;
mod hash;

define_windows_service!(ffi_service_main, service_main);

fn service_main(arguments: Vec<OsString>) {
    if let Err(e) = bootstrap_service(arguments) {
        error!("Service main failed: {}", e);
    }
}

enum BindResult {
    Bound,
    Failed(std::io::Error),
}

fn bootstrap_service(_arguments: Vec<OsString>) -> Result<(), ServerError> {
    let (shutdown_tx, shutdown_rx) = std_mpsc::channel::<()>();
    let (bind_tx, bind_rx) = std_mpsc::channel::<BindResult>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // Return NoError as a no-op.
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 1,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    })?;

    let log_path = Path::new("C:/ProgramData/Maxima/Logs/MaximaBackgroundService.log");
    std::fs::create_dir_all(log_path.safe_parent()?)?;
    let log_file = File::create(log_path)?;

    structured_logger::Builder::new()
        .with_default_writer(new_writer(log_file))
        .init();

    info!("Started Background Service");

    let actix_handle = thread::spawn(move || run_actix(bind_tx, stop_rx));

    match bind_rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| ServerError::BindTimeout)?
    {
        BindResult::Bound => {
            info!("HTTP server bound to 127.0.0.1:{}", BACKGROUND_SERVICE_PORT);
        }
        BindResult::Failed(e) => {
            error!(
                "Failed to bind HTTP server to 127.0.0.1:{}: {}",
                BACKGROUND_SERVICE_PORT, e
            );
            status_handle.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: ServiceState::Stopped,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::Win32(e.raw_os_error().unwrap_or(1) as u32),
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })?;
            let _ = stop_tx.send(());
            let _ = actix_handle.join();
            return Ok(());
        }
    }

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    let _ = shutdown_rx.recv();

    info!("Shutting down...");

    let _ = stop_tx.send(());
    let _ = actix_handle.join();

    status_handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    Ok(())
}

#[get("/set_up_registry")]
async fn req_set_up_registry() -> impl Responder {
    info!("Setting up registry");
    if let Err(err) = set_up_registry() {
        return format!("Error: {}", err);
    }

    format!("Done")
}

pub fn inject_dll(pid: u32, dll_path: &str) -> Result<(), InjectionError> {
    let injector = DllInjector::new(pid);
    injector.inject(dll_path)
}

// This is for KYBER. Ideally this would be moved to a separate Kyber service,
// but it isn't a great user experience to have to install two windows services.
// We'll eventually find a better workaround and move this somewhere else.
#[post("/inject_library")]
async fn req_inject_library(body: web::Bytes) -> Result<HttpResponse, self::ServerError> {
    info!("Injecting...");

    let req: ServiceLibraryInjectionRequest = serde_json::from_slice(&body)?;

    let hash_result = get_sha256_hash_of_pid(req.pid)?;

    // Ensure we're only injecting into STAR WARS Battlefront 2. Ideally we would check
    // the hash of the dll as well, but there isn't a great way to do that since there
    // are multiple release channels and dev builds.
    if hex::encode(hash_result)
        != "7880e40d79e981b064baaf06f10785601222c6e227a656b70112c24b1f82e2ce"
    {
        return Err(self::ServerError::InvalidInjectionTarget);
    }

    inject_dll(req.pid, &req.path)?;

    Ok(HttpResponse::Ok().body("Injected"))
}

fn run_actix(bind_tx: std_mpsc::Sender<BindResult>, stop_rx: oneshot::Receiver<()>) {
    actix_web::rt::System::new().block_on(async {
        use actix_web::{App, HttpServer};

        let http_server = match HttpServer::new(|| {
            App::new()
                .service(req_set_up_registry)
                .service(req_inject_library)
        })
        .bind(("127.0.0.1", BACKGROUND_SERVICE_PORT))
        {
            Ok(s) => s,
            Err(e) => {
                let _ = bind_tx.send(BindResult::Failed(e));
                return;
            }
        };

        let server_future = http_server.run();
        let handle = server_future.handle();

        if bind_tx.send(BindResult::Bound).is_err() {
            // The main service thread is gone; stop the server.
            handle.stop(true).await;
            return;
        }

        let stop_future = async {
            let _ = stop_rx.await;
            handle.stop(true).await;
        };

        tokio::select! {
            _ = server_future => {}
            _ = stop_future => {}
        }
    });
}

pub fn start_service() -> Result<(), self::ServerError> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}