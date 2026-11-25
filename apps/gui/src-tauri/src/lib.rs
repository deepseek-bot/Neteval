use std::path::PathBuf;

use client_core::logger;
use client_core::orchestrator::Orchestrator;
use protocol::{
    AssessmentReport, AssessmentState, NatTaskInput, P2pTaskInput, ProbeError, ReportSummary,
};
use tauri::{Emitter, Manager};

pub struct GuiState {
    orchestrator: Orchestrator,
}

impl GuiState {
    pub fn new(orchestrator: Orchestrator) -> Self {
        Self { orchestrator }
    }

    pub async fn start_nat(&self, input: NatTaskInput) -> Result<String, ProbeError> {
        self.orchestrator.start_nat_assessment(input).await
    }

    pub async fn start_p2p(&self, input: P2pTaskInput) -> Result<String, ProbeError> {
        self.orchestrator.start_p2p_assessment(input).await
    }

    pub async fn cancel(&self, reason: Option<String>) -> Result<(), ProbeError> {
        self.orchestrator.cancel_current(reason).await
    }

    pub fn history(&self) -> Result<Vec<ReportSummary>, ProbeError> {
        self.orchestrator.list_history()
    }

    pub fn report(&self, id: &str) -> Result<AssessmentReport, ProbeError> {
        self.orchestrator.get_report(id)
    }

    pub fn remove_report(&self, id: &str) -> Result<(), ProbeError> {
        self.orchestrator.delete_report(id)
    }

    pub fn current_state(&self) -> AssessmentState {
        self.orchestrator.state()
    }
}

#[tauri::command]
async fn start_nat_assessment(
    state: tauri::State<'_, GuiState>,
    input: NatTaskInput,
) -> Result<String, ProbeError> {
    state.start_nat(input).await
}

#[tauri::command]
async fn start_p2p_assessment(
    state: tauri::State<'_, GuiState>,
    input: P2pTaskInput,
) -> Result<String, ProbeError> {
    state.start_p2p(input).await
}

#[tauri::command]
async fn cancel_current(
    state: tauri::State<'_, GuiState>,
    reason: Option<String>,
) -> Result<(), ProbeError> {
    state.cancel(reason).await
}

#[tauri::command]
fn list_history(state: tauri::State<'_, GuiState>) -> Result<Vec<ReportSummary>, ProbeError> {
    state.history()
}

#[tauri::command]
fn get_report(
    state: tauri::State<'_, GuiState>,
    id: String,
) -> Result<AssessmentReport, ProbeError> {
    state.report(&id)
}

#[tauri::command]
fn delete_report(state: tauri::State<'_, GuiState>, id: String) -> Result<(), ProbeError> {
    state.remove_report(&id)
}

#[tauri::command]
fn get_current_state(state: tauri::State<'_, GuiState>) -> AssessmentState {
    state.current_state()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            let base_dir = app.path().app_local_data_dir().unwrap_or_else(|err| {
                logger::warn("gui", format!("failed to resolve app_local_data_dir, using '.': {err}"));
                PathBuf::from(".")
            });
            let log_path = base_dir.join("logs").join("latest.log");
            if let Err(err) = logger::init_logger(&log_path, false, false) {
                eprintln!("WARN [gui] failed to initialize file logger at {log_path:?}: {err}");
            }

            let reports_dir = base_dir.join("reports");
            let orchestrator = Orchestrator::new(&reports_dir);
            let mut rx = orchestrator.subscribe();

            app.manage(GuiState::new(orchestrator));

            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let initial = rx.borrow_and_update().clone();
                if let Err(err) = app_handle.emit("assessment://state", initial) {
                    logger::warn("gui", format!("failed to emit initial assessment://state: {err}"));
                }
                while rx.changed().await.is_ok() {
                    let next_state = rx.borrow_and_update().clone();
                    if let Err(err) = app_handle.emit("assessment://state", next_state) {
                        logger::warn("gui", format!("failed to emit assessment://state: {err}"));
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_nat_assessment,
            start_p2p_assessment,
            cancel_current,
            list_history,
            get_report,
            delete_report,
            get_current_state
        ])
        .build(tauri::generate_context!())
        .expect("error while building NetEval GUI application");

    app.run(|app_handle, event| {
        if let tauri::RunEvent::ExitRequested { .. } = event {
            if let Some(state) = app_handle.try_state::<GuiState>() {
                let orch = state.orchestrator.clone();
                tauri::async_runtime::block_on(async move {
                    let _ = orch.cancel_current(Some("app_exit".to_string())).await;
                });
            }
        }
    });
}
